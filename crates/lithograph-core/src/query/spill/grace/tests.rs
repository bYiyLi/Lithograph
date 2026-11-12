use super::*;

mod failures;
mod lifecycle;
mod payload;
mod transaction;

fn value_row(value: i64, width: usize) -> BindingRow {
    let mut row = BindingRow::default();
    row.insert("id".to_owned(), BindingValue::Scalar(Value::Integer(value)));
    row.insert(
        "text".to_owned(),
        BindingValue::Scalar(Value::String("x".repeat(width))),
    );
    row
}

fn append(
    store: &mut GracePartitions,
    side: HashSide,
    digest: [u8; 32],
    row: BindingRow,
    control: &ExecutionControl<'_>,
) -> i64 {
    let memory = control
        .reserve(binding_row_bytes(&row, control).unwrap())
        .unwrap();
    let partition = store.route_digest(&digest).unwrap();
    store
        .append_reserved(partition, side, digest, row, memory, control)
        .unwrap()
}

fn seeded_build_store(
    resources: &QueryResources,
    control: &ExecutionControl<'_>,
    digest: [u8; 32],
    row: BindingRow,
) -> GracePartitions {
    let mut store = GracePartitions::create(resources.clone(), control).unwrap();
    append(&mut store, HashSide::Build, digest, row, control);
    store
}

fn append_build_reserved(
    store: &mut GracePartitions,
    row: BindingRow,
    owner: &QueryResources,
    control: &ExecutionControl<'_>,
) -> QueryResult<i64> {
    let memory = owner.reserve(binding_row_bytes(&row, control)?)?;
    store.append_reserved(
        PartitionId::ROOT,
        HashSide::Build,
        [0; 32],
        row,
        memory,
        control,
    )
}

fn limit(rows: usize, bytes: u64) -> SpillReadLimit {
    SpillReadLimit { rows, bytes }
}

fn fixture_resources(row: &BindingRow) -> QueryResources {
    let sizing = QueryResources::default();
    let control = ExecutionControl::new(&sizing, &|| false);
    let payload = binding_row_bytes(row, &control).unwrap();
    let scratch = serialization_bytes(payload).unwrap();
    // Two live rows/codec workspaces, the entire bounded directory/cache,
    // and one maximal page's entry slots; no graph-size-derived allowance.
    let quota = resident_bound()
        + 2 * (payload + scratch)
        + allocation_bytes::<SpilledBinding>(MAX_PAGE_ROWS).unwrap();
    eprintln!(
        "Grace fixture payload={payload} encoding={scratch} store_bound={} quota={quota}",
        resident_bound()
    );
    QueryResources::new(Some(quota))
}

fn assert_closed(store: &GracePartitions) {
    assert!(store.connection.is_none());
    assert!(store.route_digest(&[0; 32]).is_err());
}

#[test]
fn successful_stream_is_larger_than_quota_without_retaining_all_rows() {
    let resources = fixture_resources(&value_row(0, 256));
    let quota = resources.limit().unwrap();
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    for id in 0..2048 {
        append(
            &mut store,
            HashSide::Build,
            [0; 32],
            value_row(id, 256),
            &control,
        );
    }
    let resident = 2048 * binding_row_bytes(&value_row(0, 256), &control).unwrap();
    assert!(resident > quota);
    assert!(store.stats(PartitionId::ROOT).unwrap().encoded_bytes[0] > quota);
    assert!(store.temp_logical_bytes(&control).unwrap() > quota);
    let baseline = resources.used();
    let mut after = -1;
    let mut seen = 0;
    loop {
        let page = store
            .read_batch_reserved(
                PartitionId::ROOT,
                HashSide::Build,
                after,
                limit(999, 16 * 1024),
                &control,
            )
            .unwrap();
        if page.len() == 0 {
            break;
        }
        assert!(page.len() <= MAX_PAGE_ROWS);
        assert!(resources.used() - baseline <= 16 * 1024);
        after = page.after;
        for item in page {
            assert_eq!(item.row, value_row(seen, 256));
            assert_eq!(item.seq, seen);
            seen += 1;
        }
        assert_eq!(resources.used(), baseline);
    }
    assert_eq!(seen, 2048);
    let counters = store.finish().unwrap();
    assert_eq!(counters.read_rows, [2048, 0]);
    assert!(counters.sqlite_vm_steps > 2048);
    assert_eq!(counters.sqlite_fullscan_steps, 0);
    assert_eq!(counters.sqlite_sorts, 0);
    assert!(resources.peak() <= quota);
    assert_eq!(resources.used(), 0);
}

#[test]
fn page_row_cap_and_single_large_payload_keep_original_quota() {
    let resources = QueryResources::new(Some(256 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    for _ in 0..300 {
        append(
            &mut store,
            HashSide::Build,
            [0; 32],
            BindingRow::default(),
            &control,
        );
    }
    let baseline = resources.used();
    let mut page = store
        .read_batch_reserved(
            PartitionId::ROOT,
            HashSide::Build,
            -1,
            limit(999, u64::MAX),
            &control,
        )
        .unwrap();
    assert_eq!(page.len(), MAX_PAGE_ROWS);
    let bytes = page.resident_bytes().unwrap();
    assert_eq!(bytes, resources.used() - baseline);
    let first = page.next().unwrap();
    assert_eq!(page.resident_bytes().unwrap(), bytes - first.memory.bytes());
    assert_eq!(
        resources.used() - baseline,
        page.resident_bytes().unwrap() + first.memory.bytes()
    );
    drop(first);
    drop(page);
    let page = store
        .read_batch_reserved(
            PartitionId::ROOT,
            HashSide::Build,
            -1,
            limit(256, 1),
            &control,
        )
        .unwrap();
    assert_eq!(page.len(), 1, "one legal oversized row must make progress");
    assert!(page._memory.bytes() > 1);
    drop(page);
    store.abort().unwrap();
    assert_eq!(resources.used(), 0);
}

#[test]
fn empty_binding_rows_do_not_exceed_page_target_at_vector_growth() {
    let resources = QueryResources::new(Some(512 * 1024));
    let control = ExecutionControl::new(&resources, &|| false);
    let mut store = GracePartitions::create(resources.clone(), &control).unwrap();
    for _ in 0..600 {
        append(
            &mut store,
            HashSide::Build,
            [0; 32],
            BindingRow::default(),
            &control,
        );
    }
    let baseline = resources.used();
    let mut after = -1;
    let mut seen = 0;
    loop {
        let page = store
            .read_batch_reserved(
                PartitionId::ROOT,
                HashSide::Build,
                after,
                limit(256, 64 * 1024),
                &control,
            )
            .unwrap();
        assert!(resources.used() - baseline <= 64 * 1024);
        if page.len() == 0 {
            break;
        }
        after = page.after;
        seen += page.len();
        drop(page);
        assert_eq!(resources.used(), baseline);
    }
    assert_eq!(seen, 600);
    store.abort().unwrap();
    assert_eq!(resources.used(), 0);
}
