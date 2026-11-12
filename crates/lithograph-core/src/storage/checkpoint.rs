//! Rebuildable snapshot checkpoints.

use std::collections::{BTreeMap, BTreeSet};

use rusqlite::{Connection, OptionalExtension, params};
use serde_json::{Value as JsonValue, json};

use super::{HashId, LabelId, RelationshipTypeId, StorageError, StorageResult};

mod apply;
mod copy;
mod policy;
use apply::apply_layer;
use copy::copy_checkpoint;
pub(crate) use policy::maybe_auto_checkpoint;

/// Scope query-local pins across SQL adapter wrappers and concurrent batches.
/// File copies may retain a database UUID, so the main filename participates.
pub(crate) fn snapshot_pin_identity(connection: &Connection) -> StorageResult<HashId> {
    let path = connection.path().unwrap_or("");
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"lithograph-query-snapshot-pin\0");
    hasher.update(&(path.len() as u64).to_le_bytes());
    hasher.update(path.as_bytes());
    let exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name='_lithograph_meta')",
        [], |row| row.get(0),
    )?;
    let found = if exists {
        connection
            .query_row(
                "SELECT CASE WHEN length(database_id)<=256 THEN database_id ELSE NULL END \
             FROM main._lithograph_meta WHERE id=1",
                [],
                |row| {
                    let value = row.get_ref(0)?.as_str().map_err(rusqlite::Error::from)?;
                    if value.is_empty() {
                        return Err(rusqlite::Error::InvalidQuery);
                    }
                    hasher.update(b"uuid\0");
                    hasher.update(value.as_bytes());
                    Ok(())
                },
            )
            .optional()?
            .is_some()
    } else {
        false
    };
    if !found {
        // Bare Core storage can own canonical history before the application
        // metadata bootstrap. Its filename remains stable across connections.
        if path.is_empty() {
            hasher.update(b"bare-memory\0");
            hasher.update(&(connection as *const Connection as usize).to_le_bytes());
        } else {
            hasher.update(b"bare-file\0");
        }
    }
    Ok(HashId::from_bytes(*hasher.finalize().as_bytes()))
}

// This is an internal maintenance budget, not a query memory limit. Deep
// histories remain readable even when a checkpoint build is deferred.
const MAX_REPLAY_LAYERS: usize = 100_000;
const MAX_BASE_METADATA_BYTES: i64 = 256 * 1024;
const MAX_STATISTICS_GROUPS: usize = 4096;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SnapshotStatistics {
    pub(crate) node_count: u64,
    pub(crate) relationship_count: u64,
    pub(crate) label_counts: BTreeMap<LabelId, u64>,
    pub(crate) type_counts: BTreeMap<RelationshipTypeId, u64>,
}

/// Materializes a derived checkpoint for any resolvable Commit.
pub fn create_checkpoint(connection: &Connection, commit: HashId) -> StorageResult<()> {
    create_checkpoint_checked(connection, commit, &|| Ok(()))
}

/// Build only inside an already authorized SQLite writer/maintenance boundary.
/// Cancellation is checked during lineage discovery and between SQLite writes.
pub(crate) fn create_checkpoint_checked(
    connection: &Connection,
    commit: HashId,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    check()?;
    if usable_checkpoint(connection, commit)? {
        return Ok(());
    }
    with_savepoint(connection, || {
        build_checkpoint(connection, commit, check).map(|_| ())
    })
}

fn build_checkpoint(
    connection: &Connection,
    commit: HashId,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<Option<HashId>> {
    check()?;
    // Only a complete first-parent ancestor is eligible as a base.
    let (base, layers) = first_parent_layers(connection, commit, check)?;
    #[cfg(feature = "test-support")]
    crate::performance::record_checkpoint_build(layers.len(), base.is_some());
    initialize_checkpoint_rows(connection, commit)?;
    if let Some(base) = base {
        copy_checkpoint(connection, base, commit, check)?;
    }
    for layer_id in layers {
        check()?;
        apply_layer(connection, commit, layer_id, check)?;
    }
    check()?;
    let statistics = checkpoint_statistics(connection, commit, check)?;
    check()?;
    write_statistics(connection, commit, &statistics)?;
    Ok(base)
}

fn initialize_checkpoint_rows(connection: &Connection, commit: HashId) -> StorageResult<()> {
    // Rebuild an incomplete destination only inside the surrounding savepoint.
    delete_checkpoint_rows(connection, commit)?;
    let created_at: i64 = connection.query_row(
        "SELECT CAST(unixepoch('subsec') * 1000000 AS INTEGER)",
        [],
        |row| row.get(0),
    )?;
    connection.execute(
        "INSERT INTO main._lithograph_checkpoints(commit_id, created_at, metadata) VALUES(?1, ?2, NULL)",
        params![commit.as_bytes().as_slice(), created_at],
    )?;
    Ok(())
}

/// Deletes only derived checkpoint rows; canonical history is untouched.
pub fn delete_checkpoint(connection: &Connection, commit: HashId) -> StorageResult<()> {
    with_savepoint(connection, || delete_checkpoint_rows(connection, commit))
}

fn delete_checkpoint_rows(connection: &Connection, commit: HashId) -> StorageResult<()> {
    for table in [
        "_lithograph_cp_properties",
        "_lithograph_cp_relationships",
        "_lithograph_cp_labels",
        "_lithograph_cp_nodes",
        "_lithograph_checkpoints",
    ] {
        let sql = format!("DELETE FROM main.{table} WHERE commit_id = ?1");
        connection.execute(&sql, [commit.as_bytes().as_slice()])?;
    }
    Ok(())
}

pub(super) fn usable_checkpoint(connection: &Connection, commit: HashId) -> StorageResult<bool> {
    // Untrusted derived metadata is not allowed an unbounded allocation.
    // Check the type first: length(TEXT) walks the entire value.
    let metadata: Option<Vec<u8>> = connection
        .query_row(
            "SELECT CASE WHEN typeof(metadata) = 'blob' THEN \
                    CASE WHEN length(metadata) <= ?2 THEN metadata END END \
             FROM main._lithograph_checkpoints WHERE commit_id = ?1",
            params![commit.as_bytes().as_slice(), MAX_BASE_METADATA_BYTES],
            |row| row.get(0),
        )
        .optional()?
        .flatten();
    Ok(metadata.as_deref().and_then(decode_statistics).is_some())
}

fn first_parent_layers(
    connection: &Connection,
    commit: HashId,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<(Option<HashId>, Vec<i64>)> {
    let mut current = Some(commit);
    let mut layers = Vec::new();
    let mut visited = BTreeSet::new();
    let mut statement = connection.prepare(
        "SELECT c.parent1, c.layer_id, \
                CASE WHEN typeof(cp.metadata) = 'blob' THEN \
                     CASE WHEN length(cp.metadata) <= ?2 THEN cp.metadata END END \
         FROM main._lithograph_commits AS c \
         LEFT JOIN main._lithograph_checkpoints AS cp ON cp.commit_id = c.id WHERE c.id = ?1",
    )?;
    while let Some(next) = current {
        check()?;
        if !visited.insert(next) {
            return Err(StorageError::corrupt("checkpoint lineage contains a cycle"));
        }
        let (parent, layer_id, metadata): (Option<Vec<u8>>, i64, Option<Vec<u8>>) = statement
            .query_row(
                params![next.as_bytes().as_slice(), MAX_BASE_METADATA_BYTES],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| StorageError::not_found(format!("Commit {}", next.to_hex())))?;
        if metadata.as_deref().and_then(decode_statistics).is_some() {
            layers.reverse();
            return Ok((Some(next), layers));
        }
        if layers.len() == MAX_REPLAY_LAYERS {
            return Err(StorageError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
                Some("checkpoint reconstruction exceeded the layer budget".into()),
            )));
        }
        layers.push(layer_id);
        current = parent.as_deref().map(HashId::from_slice).transpose()?;
    }
    layers.reverse();
    Ok((None, layers))
}

fn checkpoint_statistics(
    connection: &Connection,
    commit: HashId,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<SnapshotStatistics> {
    let commit = commit.as_bytes().as_slice();
    let node_count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_cp_nodes WHERE commit_id = ?1",
        [commit],
        |row| row.get(0),
    )?;
    let relationship_count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_cp_relationships WHERE commit_id = ?1",
        [commit],
        |row| row.get(0),
    )?;
    Ok(SnapshotStatistics {
        node_count: u64::try_from(node_count)
            .map_err(|_| StorageError::corrupt("checkpoint Node count is negative"))?,
        relationship_count: u64::try_from(relationship_count)
            .map_err(|_| StorageError::corrupt("checkpoint Relationship count is negative"))?,
        label_counts: grouped_counts(
            connection,
            "SELECT label_id, count(*) FROM main._lithograph_cp_labels WHERE commit_id = ?1 GROUP BY label_id",
            commit,
            check,
        )?,
        type_counts: grouped_counts(
            connection,
            "SELECT type_id, count(*) FROM main._lithograph_cp_relationships WHERE commit_id = ?1 GROUP BY type_id",
            commit,
            check,
        )?,
    })
}

fn grouped_counts(
    connection: &Connection,
    sql: &str,
    commit: &[u8],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<BTreeMap<i64, u64>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([commit], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut counts = BTreeMap::new();
    for row in rows {
        check()?;
        if counts.len() == MAX_STATISTICS_GROUPS {
            return Err(statistics_budget_error());
        }
        let (id, count) = row?;
        counts.insert(
            id,
            u64::try_from(count)
                .map_err(|_| StorageError::corrupt("checkpoint grouped count is negative"))?,
        );
    }
    Ok(counts)
}

fn write_statistics(
    connection: &Connection,
    commit: HashId,
    statistics: &SnapshotStatistics,
) -> StorageResult<()> {
    let metadata = encode_statistics(statistics);
    if metadata.len() > MAX_BASE_METADATA_BYTES as usize {
        return Err(statistics_budget_error());
    }
    connection.execute(
        "UPDATE main._lithograph_checkpoints SET metadata = ?2 WHERE commit_id = ?1",
        params![commit.as_bytes().as_slice(), metadata],
    )?;
    Ok(())
}

pub(crate) fn load_checkpoint_statistics(
    connection: &Connection,
    commit: HashId,
) -> StorageResult<Option<SnapshotStatistics>> {
    let metadata = connection.query_row(
        "SELECT CASE WHEN typeof(metadata) = 'blob' THEN \
                CASE WHEN length(metadata) <= ?2 THEN metadata END END \
         FROM main._lithograph_checkpoints WHERE commit_id = ?1",
        params![commit.as_bytes().as_slice(), MAX_BASE_METADATA_BYTES],
        |row| row.get::<_, Option<Vec<u8>>>(0),
    )?;
    Ok(metadata.as_deref().and_then(decode_statistics))
}

fn encode_statistics(statistics: &SnapshotStatistics) -> Vec<u8> {
    json!({
        "version": 1,
        "nodes": statistics.node_count,
        "relationships": statistics.relationship_count,
        "labels": encode_counts(&statistics.label_counts),
        "types": encode_counts(&statistics.type_counts),
    })
    .to_string()
    .into_bytes()
}

fn encode_counts(counts: &BTreeMap<i64, u64>) -> Vec<JsonValue> {
    counts
        .iter()
        .map(|(id, count)| json!([id, count]))
        .collect()
}

fn decode_statistics(metadata: &[u8]) -> Option<SnapshotStatistics> {
    if metadata.len() > MAX_BASE_METADATA_BYTES as usize {
        return None;
    }
    let value: JsonValue = serde_json::from_slice(metadata).ok()?;
    if value.get("version")?.as_u64()? != 1 {
        return None;
    }
    let statistics = SnapshotStatistics {
        node_count: value.get("nodes")?.as_u64()?,
        relationship_count: value.get("relationships")?.as_u64()?,
        label_counts: decode_counts(value.get("labels")?)?,
        type_counts: decode_counts(value.get("types")?)?,
    };
    if !consistent_totals(&statistics) {
        return None;
    }
    Some(statistics)
}

fn consistent_totals(statistics: &SnapshotStatistics) -> bool {
    statistics
        .label_counts
        .values()
        .all(|count| *count <= statistics.node_count)
        && statistics
            .type_counts
            .values()
            .try_fold(0_u64, |sum, count| sum.checked_add(*count))
            == Some(statistics.relationship_count)
}

fn decode_counts(value: &JsonValue) -> Option<BTreeMap<i64, u64>> {
    let entries = value.as_array()?;
    let mut counts = BTreeMap::new();
    for entry in entries {
        let pair = entry.as_array()?;
        if pair.len() != 2 {
            return None;
        }
        let (id, count) = (pair[0].as_i64()?, pair[1].as_u64()?);
        if id <= 0 || count == 0 || counts.insert(id, count).is_some() {
            return None;
        }
    }
    Some(counts)
}

fn statistics_budget_error() -> StorageError {
    StorageError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_TOOBIG),
        Some("checkpoint statistics exceeded the maintenance budget".into()),
    ))
}

fn with_savepoint<T>(
    connection: &Connection,
    operation: impl FnOnce() -> StorageResult<T>,
) -> StorageResult<T> {
    connection.execute_batch("SAVEPOINT lithograph_checkpoint_write")?;
    match operation() {
        Ok(value) => match connection.execute_batch("RELEASE lithograph_checkpoint_write") {
            Ok(()) => Ok(value),
            Err(error) => {
                // A failed RELEASE is not proof the savepoint was closed.
                // Never defer its resource error until its rollback succeeds.
                rollback_checkpoint_savepoint(connection)?;
                Err(StorageError::from(error))
            }
        },
        Err(error) => {
            rollback_checkpoint_savepoint(connection)?;
            Err(error)
        }
    }
}

fn rollback_checkpoint_savepoint(connection: &Connection) -> StorageResult<()> {
    connection
        .execute_batch(
            "ROLLBACK TO lithograph_checkpoint_write; RELEASE lithograph_checkpoint_write",
        )
        .map_err(|error| {
            // Auto-maintenance may skip SQLITE_FULL/BUSY only when it knows
            // that all partial checkpoint writes have been rolled back.
            StorageError::corrupt(format!(
                "checkpoint savepoint rollback failed; transaction state is uncertain: {error}"
            ))
        })
}

#[cfg(test)]
mod tests;
