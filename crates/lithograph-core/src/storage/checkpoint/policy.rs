//! Opportunistic checkpoint maintenance at the existing canonical writer boundary.
//!
//! Sampling avoids a history walk on every Commit. Both the observed delta
//! work and the candidate materialization must be small enough to fit an
//! ordinary writer transaction; larger graphs can still use explicit
//! maintenance, with no hidden query-time writes.

use rusqlite::{Connection, OptionalExtension, Statement, ToSql, params};

use super::{HashId, StorageError, StorageResult, decode_statistics};
use crate::storage::SnapshotReadObserver;

// The content-addressed Commit gives independent sampling across Branches,
// unlike the global Layer sequence (which could starve one interleaved Branch).
const COMMIT_SAMPLE_MASK: u8 = 0x0f;
const MIN_CHAIN_DEPTH: usize = 128;
const FORCE_CHAIN_DEPTH: usize = 512;
const MAX_CHAIN_DEPTH: usize = 1024;
const MIN_REPLAY_BYTES: u64 = 4 * 1024 * 1024;
const MAX_AUTO_CHECKPOINT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_AUTO_CHECKPOINTS: i64 = 4;
const MAX_METADATA_BYTES: i64 = 256 * 1024;

type LineageRow = (Option<Vec<u8>>, i64, Option<Vec<u8>>);

struct ReplayCost {
    depth: usize,
    bytes: u64,
    base: Option<(HashId, super::SnapshotStatistics)>,
}

const LINEAGE_SQL: &str = r#"
    SELECT c.parent1, c.layer_id,
           CASE WHEN typeof(cp.metadata) = 'blob' THEN
                CASE WHEN length(cp.metadata) <= ?2 THEN cp.metadata END END
    FROM main._lithograph_commits AS c
    LEFT JOIN main._lithograph_checkpoints AS cp ON cp.commit_id = c.id
    WHERE c.id = ?1
"#;

/// Called only after a successful canonical compare-and-move inside its
/// existing savepoint. No independent connection, transaction, Commit or ref
/// mutation is introduced for this derived-data maintenance.
pub(crate) fn maybe_auto_checkpoint(
    connection: &Connection,
    commit: HashId,
    observer: Option<&dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    if commit.as_bytes()[0] & COMMIT_SAMPLE_MASK != 0 {
        return Ok(());
    }
    let candidate = match should_checkpoint(connection, commit, check) {
        Ok(candidate) => candidate,
        Err(error) => return defer_maintenance_resource_error(error),
    };
    if candidate {
        // A derived maintenance resource limit must not veto an otherwise
        // valid canonical Commit. The checkpoint writer already rolled its
        // own savepoint back before returning an error.
        if let Err(error) = publish_automatic_checkpoint(connection, commit, observer, check) {
            return defer_maintenance_resource_error(error);
        }
    }
    Ok(())
}

pub(super) fn publish_automatic_checkpoint(
    connection: &Connection,
    commit: HashId,
    observer: Option<&dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    check()?;
    if super::usable_checkpoint(connection, commit)? {
        return Ok(());
    }
    // Build, identify policy-owned data, and evict it in one savepoint. Manual
    // checkpoints are never adopted or evicted by opportunistic maintenance.
    super::with_savepoint(connection, || {
        let base = super::build_checkpoint(connection, commit, check)?;
        connection.execute(
            "UPDATE main._lithograph_checkpoints SET metadata = \
             CAST(json_set(CAST(metadata AS TEXT), '$.automatic', json('true')) AS BLOB) \
             WHERE commit_id = ?1",
            [commit.as_bytes().as_slice()],
        )?;
        prune_automatic_checkpoints(connection, commit, base, observer, check)
    })
}

fn prune_automatic_checkpoints(
    connection: &Connection,
    published: HashId,
    base: Option<HashId>,
    observer: Option<&dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    check()?;
    let mut statement = connection.prepare(
        "SELECT commit_id FROM main._lithograph_checkpoints \
         WHERE CASE WHEN typeof(metadata) = 'blob' THEN \
                    CASE WHEN length(metadata) <= ?1 AND json_valid(CAST(metadata AS TEXT)) \
                         THEN json_extract(CAST(metadata AS TEXT), '$.automatic') = 1 ELSE 0 END \
               ELSE 0 END \
         ORDER BY created_at, commit_id LIMIT ?2",
    )?;
    // At most four old automatic checkpoints and the just-published one.
    let candidates = statement
        .query_map(
            params![MAX_METADATA_BYTES, MAX_AUTO_CHECKPOINTS + 1],
            |row| row.get::<_, Vec<u8>>(0),
        )?
        .map(|id| HashId::from_slice(&id?))
        .collect::<StorageResult<Vec<_>>>()?;
    drop(statement);
    if candidates.len() <= MAX_AUTO_CHECKPOINTS as usize {
        return check();
    }
    // The current operation may still lazily project through its immutable
    // first-parent base after publication. Its own SQLite writes are visible
    // even under a read guard, so retain that base until a later operation.
    let database = observer
        .map(|_| super::snapshot_pin_identity(connection))
        .transpose()?;
    for id in candidates {
        check()?;
        if protected_checkpoint(id, published, base, observer, database)? {
            continue;
        }
        super::delete_checkpoint_rows(connection, id)?;
        return check();
    }
    // No safe eviction is available. The caller must roll back the entire
    // newly-built checkpoint before deferring this optional maintenance.
    Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
        Some("automatic checkpoint has no unpinned eviction candidate".into()),
    )))
}

fn protected_checkpoint(
    id: HashId,
    published: HashId,
    base: Option<HashId>,
    observer: Option<&dyn SnapshotReadObserver>,
    database: Option<HashId>,
) -> StorageResult<bool> {
    if id == published || Some(id) == base {
        return Ok(true);
    }
    match (observer, database) {
        (Some(observer), Some(database)) => observer.checkpoint_is_pinned(database, id),
        _ => Ok(false),
    }
}

fn defer_maintenance_resource_error(error: StorageError) -> StorageResult<()> {
    match &error {
        StorageError::Sqlite(rusqlite::Error::SqliteFailure(sqlite, _))
            if matches!(
                sqlite.code,
                rusqlite::ErrorCode::DiskFull
                    | rusqlite::ErrorCode::OutOfMemory
                    | rusqlite::ErrorCode::TooBig
                    | rusqlite::ErrorCode::DatabaseBusy
            ) =>
        {
            Ok(())
        }
        // Interrupt, corruption, storage I/O failures, and canonical/history
        // errors remain visible to the existing writer rollback boundary.
        _ => Err(error),
    }
}

fn should_checkpoint(
    connection: &Connection,
    commit: HashId,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<bool> {
    let Some(cost) = estimate_replay_cost(connection, commit, check)? else {
        return Ok(false);
    };
    if cost.depth < MIN_CHAIN_DEPTH
        || (cost.depth < FORCE_CHAIN_DEPTH && cost.bytes < MIN_REPLAY_BYTES)
    {
        return Ok(false);
    }
    let base_bytes = match cost.base {
        Some((id, stats)) => {
            let Some(bytes) = estimate_checkpoint_size(connection, id, &stats)? else {
                return Ok(false);
            };
            bytes
        }
        None => 0,
    };
    Ok(base_bytes.saturating_add(cost.bytes) <= MAX_AUTO_CHECKPOINT_BYTES)
}

fn estimate_replay_cost(
    connection: &Connection,
    commit: HashId,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<Option<ReplayCost>> {
    let mut current = Some(commit);
    let mut depth = 0;
    let mut delta_bytes = 0_u64;
    let mut base = None;
    let mut statement = connection.prepare(LINEAGE_SQL)?;
    while depth < MAX_CHAIN_DEPTH {
        let Some(next) = current else { break };
        check()?;
        let (parent, layer_id, metadata) = load_lineage_row(&mut statement, next)?;
        if let Some(stats) = metadata.as_deref().and_then(decode_statistics) {
            base = Some((next, stats));
            break;
        }
        depth += 1;
        let Some(bytes) = layer_cost(
            connection,
            layer_id,
            MAX_AUTO_CHECKPOINT_BYTES - delta_bytes,
        )?
        else {
            return Ok(None);
        };
        delta_bytes += bytes;
        current = parent.as_deref().map(HashId::from_slice).transpose()?;
    }
    // Do not estimate the size of a derived ancestor we never inspected.
    // If this history is too deep to inspect cheaply, defer maintenance.
    if depth == MAX_CHAIN_DEPTH && base.is_none() && current.is_some() {
        return Ok(None);
    }
    Ok(Some(ReplayCost {
        depth,
        bytes: delta_bytes,
        base,
    }))
}

fn load_lineage_row(statement: &mut Statement<'_>, commit: HashId) -> StorageResult<LineageRow> {
    statement
        .query_row(
            params![commit.as_bytes().as_slice(), MAX_METADATA_BYTES],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?
        .ok_or_else(|| StorageError::not_found(format!("Commit {}", commit.to_hex())))
}

fn layer_cost(connection: &Connection, layer_id: i64, budget: u64) -> StorageResult<Option<u64>> {
    let mut bytes = 0;
    for (table, unit) in [
        ("_lithograph_node_delta", 32),
        ("_lithograph_label_delta", 40),
        ("_lithograph_rel_delta", 96),
    ] {
        let sql =
            format!("SELECT count(*) FROM (SELECT 1 FROM main.{table} WHERE layer_id=?1 LIMIT ?2)");
        let remaining = budget - bytes;
        let rows: i64 = connection
            .prepare(&sql)?
            .query_row(params![layer_id, (remaining / unit + 1) as i64], |row| {
                row.get(0)
            })?;
        let count_bytes = rows as u64 * unit;
        if count_bytes > remaining {
            return Ok(None);
        }
        bytes += count_bytes;
    }
    Ok(property_cost(
        connection,
        "_lithograph_property_delta",
        "layer_id",
        &layer_id,
        budget - bytes,
    )?
    .map(|properties| bytes + properties))
}

fn estimate_checkpoint_size(
    connection: &Connection,
    id: HashId,
    stats: &super::SnapshotStatistics,
) -> StorageResult<Option<u64>> {
    let label_count: u64 = stats
        .label_counts
        .values()
        .fold(0_u64, |count, value| count.saturating_add(*value));
    let graph_bytes = stats
        .node_count
        .saturating_mul(32)
        .saturating_add(stats.relationship_count.saturating_mul(96))
        .saturating_add(label_count.saturating_mul(40));
    if graph_bytes > MAX_AUTO_CHECKPOINT_BYTES {
        return Ok(None);
    }
    Ok(property_cost(
        connection,
        "_lithograph_cp_properties",
        "commit_id",
        &id.as_bytes().as_slice(),
        MAX_AUTO_CHECKPOINT_BYTES - graph_bytes,
    )?
    .map(|properties| graph_bytes + properties))
}

fn property_cost(
    connection: &Connection,
    table: &str,
    selector: &str,
    key: &dyn ToSql,
    budget: u64,
) -> StorageResult<Option<u64>> {
    // Every row costs at least 96 bytes: inspect at most the remaining byte
    // budget / 96 + one sentinel row. Compute lengths inside the subquery so
    // SQLite's octet_length/length opcodes read overflow-value size headers,
    // instead of materializing giant TEXT/BLOB values in the coroutine.
    let sql = format!(
        "SELECT coalesce(sum(bytes),0) FROM (\
         SELECT min(?3,96+4*coalesce(octet_length(text_value),0)+\
             coalesce(length(blob_value),0)+coalesce(length(aux_value),0)) AS bytes \
         FROM main.{table} WHERE {selector}=?1 LIMIT ?2)"
    );
    let bytes: i64 = connection.prepare(&sql)?.query_row(
        params![key, (budget / 96 + 1) as i64, (budget + 1) as i64],
        |row| row.get(0),
    )?;
    let bytes = bytes as u64;
    Ok((bytes <= budget).then_some(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    };

    fn property_fixture(rows: usize) -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection.execute_batch(
            "CREATE TABLE estimate_properties(scope INTEGER,text_value TEXT,blob_value BLOB,aux_value BLOB);\
             CREATE INDEX estimate_scope ON estimate_properties(scope);"
        ).unwrap();
        connection.execute(
            "WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers WHERE n<?1) \
             INSERT INTO estimate_properties(scope) SELECT 1 FROM numbers",
            [rows as i64],
        ).unwrap();
        connection
    }

    #[test]
    fn property_estimation_stops_at_its_remaining_budget_instead_of_scanning_the_table() {
        let mut work = Vec::new();
        for rows in [1000, 10000] {
            let connection = property_fixture(rows);
            let steps = Arc::new(AtomicU64::new(0));
            let observed = steps.clone();
            connection
                .progress_handler(
                    1,
                    Some(move || {
                        observed.fetch_add(1, Ordering::Relaxed);
                        false
                    }),
                )
                .unwrap();
            let cost =
                property_cost(&connection, "estimate_properties", "scope", &1, 4 * 96).unwrap();
            connection
                .progress_handler(0, None::<fn() -> bool>)
                .unwrap();
            assert_eq!(cost, None);
            work.push(steps.load(Ordering::Relaxed));
        }
        assert!(
            work[0] < 256,
            "estimator did not stop at five candidate rows: {work:?}"
        );
        assert!(
            work[1] <= work[0] + 32,
            "unrelated rows inflated estimation work: {work:?}"
        );
    }

    #[test]
    fn affordable_property_estimates_remain_usable_and_giant_values_defer() {
        let connection = property_fixture(1000);
        assert_eq!(
            property_cost(&connection, "estimate_properties", "scope", &1, 1000 * 96).unwrap(),
            Some(1000 * 96),
        );
        connection
            .execute("DELETE FROM estimate_properties", [])
            .unwrap();
        connection
            .execute(
                "INSERT INTO estimate_properties VALUES(1,?1,?2,?3)",
                params![
                    "中".repeat(1024 * 1024),
                    vec![1_u8; 1024 * 1024],
                    vec![2_u8; 1024 * 1024]
                ],
            )
            .unwrap();
        assert_eq!(
            property_cost(&connection, "estimate_properties", "scope", &1, 1024).unwrap(),
            None
        );
        assert_eq!(
            property_cost(
                &connection,
                "estimate_properties",
                "scope",
                &1,
                MAX_AUTO_CHECKPOINT_BYTES
            )
            .unwrap(),
            Some(96 + 14 * 1024 * 1024),
        );
    }

    #[test]
    fn automatic_derived_work_defers_only_explicit_resource_failures() {
        for code in [
            rusqlite::ffi::SQLITE_FULL,
            rusqlite::ffi::SQLITE_NOMEM,
            rusqlite::ffi::SQLITE_TOOBIG,
            rusqlite::ffi::SQLITE_BUSY,
        ] {
            let error = StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(defer_maintenance_resource_error(error).is_ok());
        }
        for code in [
            rusqlite::ffi::SQLITE_INTERRUPT,
            rusqlite::ffi::SQLITE_CORRUPT,
        ] {
            let error = StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(code),
                None,
            ));
            assert!(defer_maintenance_resource_error(error).is_err());
        }
        assert!(defer_maintenance_resource_error(StorageError::BranchHeadMoved).is_err());
    }
}
