use super::*;

pub(super) fn read_page(
    store: &mut GracePartitions,
    partition: PartitionId,
    side: HashSide,
    after: i64,
    limit: SpillReadLimit,
    control: &ExecutionControl<'_>,
    budget_refused: &mut bool,
) -> QueryResult<ReservedSpillBatch> {
    control.check()?;
    store.directory.stats(partition)?;
    if limit.rows == 0 || limit.bytes == 0 {
        return Err(QueryError::invalid_argument(
            "Grace read page bounds must be positive",
        ));
    }
    let resources = store.resources.clone();
    let sql = format!(
        "SELECT seq,digest,binding_json FROM temp.{} \
         WHERE partition=?1 AND side=?2 AND seq>?3 ORDER BY seq LIMIT ?4",
        store.table
    );
    let page = store.sql(&sql, |statement| {
        let rows = statement.query(params![
            partition.0,
            side as i64,
            after,
            limit.rows.min(MAX_PAGE_ROWS) as i64
        ])?;
        reader::read_rows(
            rows,
            after,
            limit.bytes,
            &resources,
            control,
            budget_refused,
        )
    })?;
    store.counters.read_batches = checked_add(store.counters.read_batches, 1)?;
    store.counters.read_rows[side as usize] =
        checked_add(store.counters.read_rows[side as usize], page.len() as u64)?;
    Ok(page)
}

pub(super) fn read_digest(row: &rusqlite::Row<'_>, index: usize) -> QueryResult<[u8; 32]> {
    let value = row.get_ref(index)?;
    let blob = value
        .as_blob()
        .map_err(|_| QueryError::internal("Grace digest is not a BLOB"))?;
    blob.try_into()
        .map_err(|_| QueryError::internal("Grace digest has an invalid length"))
}

#[derive(Debug, Clone, Copy)]
struct Key {
    seq: i64,
    digest: [u8; 32],
    bytes: u64,
}

impl Key {
    fn read(row: &rusqlite::Row<'_>) -> QueryResult<Self> {
        Ok(Self {
            seq: row.get(0)?,
            digest: read_digest(row, 1)?,
            bytes: nonnegative(row.get(2)?)?,
        })
    }
}

struct KeyPage {
    keys: Vec<Key>,
    _memory: MemoryReservation,
}

fn key_page(
    store: &mut GracePartitions,
    partition: PartitionId,
    side: HashSide,
    after: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<KeyPage> {
    control.check()?;
    let resources = store.resources.clone();
    let sql = format!(
        "SELECT seq,digest,octet_length(binding_json) FROM temp.{} \
         WHERE partition=?1 AND side=?2 AND seq>?3 ORDER BY seq LIMIT ?4",
        store.table
    );
    store.sql(&sql, |statement| {
        let mut rows = statement.query(params![
            partition.0,
            side as i64,
            after,
            MAX_PAGE_ROWS as i64
        ])?;
        let mut output = Vec::new();
        let mut memory = resources.reserve(0)?;
        let mut last = after;
        while let Some(row) = rows.next()? {
            control.work(1)?;
            let key = Key::read(row)?;
            if key.seq <= last {
                return Err(QueryError::internal("Grace key sequence is not increasing"));
            }
            if let Err(error) = reserve_vec_slot(&mut output, &mut memory) {
                if error.kind == QueryErrorKind::Resource && !output.is_empty() {
                    break;
                }
                return Err(error);
            }
            last = key.seq;
            output.push(key);
        }
        control.check()?;
        Ok(KeyPage {
            keys: output,
            _memory: memory,
        })
    })
}

pub(super) fn repartition(
    store: &mut GracePartitions,
    partition: PartitionId,
    control: &ExecutionControl<'_>,
) -> QueryResult<RepartitionOutcome> {
    control.check()?;
    let stats = store.stats(partition)?;
    if stats.depth >= MAX_DEPTH || stats.rows == [0; 2] {
        return Ok(RepartitionOutcome::Skew(partition));
    }
    let children = store.directory.split(partition)?;
    for side in [HashSide::Build, HashSide::Probe] {
        move_side(store, partition, side, children, stats.depth, control)?;
    }
    verify_totals(store, children, stats)?;
    store.directory.clear_retired(partition);
    store.counters.partitions = checked_add(store.counters.partitions, FANOUT as u64)?;
    store.counters.max_depth = store.counters.max_depth.max(stats.depth + 1);
    control.check()?;
    Ok(RepartitionOutcome::Children(children))
}

fn verify_totals(
    store: &GracePartitions,
    children: [PartitionId; FANOUT],
    expected: PartitionStats,
) -> QueryResult<()> {
    let mut observed = PartitionStats::default();
    for child in children {
        let child = store.directory.stats(child)?;
        for side in 0..2 {
            observed.rows[side] = checked_add(observed.rows[side], child.rows[side])?;
            observed.encoded_bytes[side] =
                checked_add(observed.encoded_bytes[side], child.encoded_bytes[side])?;
        }
    }
    if observed.rows != expected.rows || observed.encoded_bytes != expected.encoded_bytes {
        return Err(QueryError::internal(
            "Grace partition contents contradict recorded totals",
        ));
    }
    Ok(())
}

fn move_side(
    store: &mut GracePartitions,
    partition: PartitionId,
    side: HashSide,
    children: [PartitionId; FANOUT],
    depth: u8,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let sql = format!(
        "UPDATE temp.{} SET partition=?1 WHERE partition=?2 AND side=?3 AND seq=?4",
        store.table
    );
    let mut after = -1;
    loop {
        let page = key_page(store, partition, side, after, control)?;
        if page.keys.is_empty() {
            break;
        }
        store.counters.repartition_batches = checked_add(store.counters.repartition_batches, 1)?;
        for key in page.keys {
            control.check()?;
            let child = children[directory::bucket(&key.digest, depth)];
            store.sql(&sql, |statement| {
                let changed =
                    statement.execute(params![child.0, partition.0, side as i64, key.seq])?;
                if changed != 1 {
                    return Err(QueryError::internal("Grace repartition lost a row"));
                }
                Ok(())
            })?;
            store.directory.add(child, side, key.bytes)?;
            store.counters.repartition_rows = checked_add(store.counters.repartition_rows, 1)?;
            after = key.seq;
            control.check()?;
        }
    }
    Ok(())
}

pub(super) fn retire_probe(
    store: &mut GracePartitions,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.check()?;
    let mut leaf = store.next_leaf(None);
    while let Some(partition) = leaf {
        if store.stats(partition)?.rows[1] > 0 {
            retire_leaf(store, partition, control)?;
        }
        leaf = store.next_leaf(Some(partition));
    }
    control.check()
}

fn retire_leaf(
    store: &mut GracePartitions,
    partition: PartitionId,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let expected = store.stats(partition)?.rows[1];
    let sql = format!(
        "DELETE FROM temp.{} WHERE partition=?1 AND side=?2 AND seq=?3",
        store.table
    );
    let mut after = -1;
    let mut removed = 0;
    loop {
        let page = key_page(store, partition, HashSide::Probe, after, control)?;
        if page.keys.is_empty() {
            break;
        }
        store.counters.retired_probe_batches =
            checked_add(store.counters.retired_probe_batches, 1)?;
        for key in page.keys {
            control.check()?;
            store.sql(&sql, |statement| {
                if statement.execute(params![partition.0, HashSide::Probe as i64, key.seq])? != 1 {
                    return Err(QueryError::internal("Grace retirement lost a Probe row"));
                }
                Ok(())
            })?;
            removed = checked_add(removed, 1)?;
            after = key.seq;
            control.check()?;
        }
    }
    if removed != expected {
        return Err(QueryError::internal(
            "Grace Probe totals contradict retirement",
        ));
    }
    store.counters.retired_probe_rows = checked_add(store.counters.retired_probe_rows, removed)?;
    store.directory.clear_probe(partition);
    Ok(())
}
