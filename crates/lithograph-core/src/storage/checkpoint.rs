//! Rebuildable snapshot checkpoints.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};
use serde_json::{Value as JsonValue, json};

use super::property::PropertyColumns;
use super::snapshot::Snapshot;
use super::{HashId, LabelId, RelationshipTypeId, StorageResult};

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SnapshotStatistics {
    pub(crate) node_count: u64,
    pub(crate) relationship_count: u64,
    pub(crate) label_counts: BTreeMap<LabelId, u64>,
    pub(crate) type_counts: BTreeMap<RelationshipTypeId, u64>,
}

/// Materializes a derived checkpoint for any resolvable Commit.
pub fn create_checkpoint(connection: &Connection, commit: HashId) -> StorageResult<()> {
    if checkpoint_exists(connection, commit)? {
        return Ok(());
    }
    let snapshot = Snapshot::resolve(connection, commit)?;
    with_savepoint(connection, || {
        let created_at: i64 = connection.query_row(
            "SELECT CAST(unixepoch('subsec') * 1000000 AS INTEGER)",
            [],
            |row| row.get(0),
        )?;
        connection.execute(
            "INSERT INTO main._lithograph_checkpoints(commit_id, created_at, metadata) VALUES(?1, ?2, NULL)",
            params![commit.as_bytes().as_slice(), created_at],
        )?;
        let node_count = write_nodes(connection, commit, &snapshot)?;
        let label_counts = write_labels(connection, commit, &snapshot)?;
        let (relationship_count, type_counts) = write_relationships(connection, commit, &snapshot)?;
        write_properties(connection, commit, &snapshot)?;
        write_statistics(
            connection,
            commit,
            &SnapshotStatistics {
                node_count,
                relationship_count,
                label_counts,
                type_counts,
            },
        )
    })
}

/// Deletes only derived checkpoint rows; canonical history is untouched.
pub fn delete_checkpoint(connection: &Connection, commit: HashId) -> StorageResult<()> {
    with_savepoint(connection, || {
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
    })
}

fn write_nodes(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> StorageResult<u64> {
    let mut statement = connection
        .prepare("INSERT INTO main._lithograph_cp_nodes(commit_id, node_id) VALUES(?1, ?2)")?;
    let mut count = 0_u64;
    snapshot.visit_nodes(|node_id| {
        statement.execute(params![commit.as_bytes().as_slice(), node_id])?;
        count = count.saturating_add(1);
        Ok(())
    })?;
    Ok(count)
}

fn write_labels(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> StorageResult<BTreeMap<LabelId, u64>> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_cp_labels(commit_id, node_id, label_id) VALUES(?1, ?2, ?3)",
    )?;
    let mut counts = BTreeMap::new();
    snapshot.visit_labels(|node_id, label_id| {
        statement.execute(params![commit.as_bytes().as_slice(), node_id, label_id])?;
        let count = counts.entry(label_id).or_insert(0_u64);
        *count = count.saturating_add(1);
        Ok(())
    })?;
    Ok(counts)
}

fn write_relationships(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> StorageResult<(u64, BTreeMap<RelationshipTypeId, u64>)> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_cp_relationships(commit_id, relationship_id, source_id, type_id, target_id) VALUES(?1, ?2, ?3, ?4, ?5)",
    )?;
    let mut count = 0_u64;
    let mut type_counts = BTreeMap::new();
    snapshot.visit_relationships(|relationship| {
        statement.execute(params![
            commit.as_bytes().as_slice(),
            relationship.id,
            relationship.source,
            relationship.type_id,
            relationship.target,
        ])?;
        count = count.saturating_add(1);
        let type_count = type_counts.entry(relationship.type_id).or_insert(0_u64);
        *type_count = type_count.saturating_add(1);
        Ok(())
    })?;
    Ok((count, type_counts))
}

fn write_properties(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_cp_properties(commit_id, owner_kind, owner_id, key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10)",
    )?;
    snapshot.visit_properties(|owner_kind, owner_id, key_id, value| {
        let columns = PropertyColumns::from_value(&value)?;
        statement.execute(params![
            commit.as_bytes().as_slice(),
            owner_kind as i64,
            owner_id,
            key_id,
            columns.type_tag,
            columns.int_value,
            columns.real_value,
            columns.text_value.as_deref(),
            columns.blob_value.as_deref(),
            columns.aux_value.as_deref(),
        ])?;
        Ok(())
    })
}

fn write_statistics(
    connection: &Connection,
    commit: HashId,
    statistics: &SnapshotStatistics,
) -> StorageResult<()> {
    connection.execute(
        "UPDATE main._lithograph_checkpoints SET metadata = ?2 WHERE commit_id = ?1",
        params![commit.as_bytes().as_slice(), encode_statistics(statistics)],
    )?;
    Ok(())
}

pub(crate) fn load_checkpoint_statistics(
    connection: &Connection,
    commit: HashId,
) -> StorageResult<Option<SnapshotStatistics>> {
    let metadata = connection.query_row(
        "SELECT metadata FROM main._lithograph_checkpoints WHERE commit_id = ?1",
        [commit.as_bytes().as_slice()],
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
    let value: JsonValue = serde_json::from_slice(metadata).ok()?;
    if value.get("version")?.as_u64()? != 1 {
        return None;
    }
    Some(SnapshotStatistics {
        node_count: value.get("nodes")?.as_u64()?,
        relationship_count: value.get("relationships")?.as_u64()?,
        label_counts: decode_counts(value.get("labels")?)?,
        type_counts: decode_counts(value.get("types")?)?,
    })
}

fn decode_counts(value: &JsonValue) -> Option<BTreeMap<i64, u64>> {
    value
        .as_array()?
        .iter()
        .map(|entry| -> Option<(i64, u64)> {
            let pair = entry.as_array()?;
            if pair.len() != 2 {
                return None;
            }
            Some((pair[0].as_i64()?, pair[1].as_u64()?))
        })
        .collect()
}

fn checkpoint_exists(connection: &Connection, commit: HashId) -> StorageResult<bool> {
    let exists: i64 = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main._lithograph_checkpoints WHERE commit_id = ?1)",
        [commit.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(exists == 1)
}

fn with_savepoint<T>(
    connection: &Connection,
    operation: impl FnOnce() -> StorageResult<T>,
) -> StorageResult<T> {
    connection.execute_batch("SAVEPOINT lithograph_checkpoint_write")?;
    match operation() {
        Ok(value) => {
            connection.execute_batch("RELEASE lithograph_checkpoint_write")?;
            Ok(value)
        }
        Err(error) => {
            connection.execute_batch(
                "ROLLBACK TO lithograph_checkpoint_write; RELEASE lithograph_checkpoint_write",
            )?;
            Err(error)
        }
    }
}
