//! Rebuildable snapshot checkpoints.

use std::collections::BTreeMap;

use rusqlite::{Connection, params};
use serde_json::{Value as JsonValue, json};

use super::{HashId, LabelId, RelationshipTypeId, StorageError, StorageResult};

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
        for layer_id in first_parent_layers(connection, commit)? {
            apply_layer(connection, commit, layer_id)?;
        }
        let statistics = checkpoint_statistics(connection, commit)?;
        write_statistics(connection, commit, &statistics)
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

fn first_parent_layers(connection: &Connection, commit: HashId) -> StorageResult<Vec<i64>> {
    let mut current = Some(commit);
    let mut layers = Vec::new();
    while let Some(commit) = current {
        let (parent, layer_id): (Option<Vec<u8>>, i64) = connection
            .query_row(
                "SELECT parent1, layer_id FROM main._lithograph_commits WHERE id = ?1",
                [commit.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(StorageError::from)?;
        layers.push(layer_id);
        current = parent.as_deref().map(HashId::from_slice).transpose()?;
    }
    layers.reverse();
    Ok(layers)
}

fn apply_layer(connection: &Connection, commit: HashId, layer_id: i64) -> StorageResult<()> {
    let commit = commit.as_bytes().as_slice();
    connection.execute(
        "DELETE FROM main._lithograph_cp_nodes WHERE commit_id = ?1 AND node_id IN (SELECT node_id FROM main._lithograph_node_delta WHERE layer_id = ?2 AND op = 2)",
        params![commit, layer_id],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO main._lithograph_cp_nodes(commit_id, node_id) SELECT ?1, node_id FROM main._lithograph_node_delta WHERE layer_id = ?2 AND op = 1",
        params![commit, layer_id],
    )?;

    connection.execute(
        "DELETE FROM main._lithograph_cp_labels WHERE commit_id = ?1 AND (node_id, label_id) IN (SELECT node_id, label_id FROM main._lithograph_label_delta WHERE layer_id = ?2 AND op = 2)",
        params![commit, layer_id],
    )?;
    connection.execute(
        "INSERT OR IGNORE INTO main._lithograph_cp_labels(commit_id, node_id, label_id) SELECT ?1, node_id, label_id FROM main._lithograph_label_delta WHERE layer_id = ?2 AND op = 1",
        params![commit, layer_id],
    )?;

    connection.execute(
        "DELETE FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND relationship_id IN (SELECT relationship_id FROM main._lithograph_rel_delta WHERE layer_id = ?2 AND op = 2)",
        params![commit, layer_id],
    )?;
    connection.execute(
        "INSERT OR REPLACE INTO main._lithograph_cp_relationships(commit_id, relationship_id, source_id, type_id, target_id) SELECT ?1, relationship_id, source_id, type_id, target_id FROM main._lithograph_rel_delta WHERE layer_id = ?2 AND op = 1",
        params![commit, layer_id],
    )?;

    connection.execute(
        "DELETE FROM main._lithograph_cp_properties WHERE commit_id = ?1 AND (owner_kind, owner_id, key_id) IN (SELECT owner_kind, owner_id, key_id FROM main._lithograph_property_delta WHERE layer_id = ?2 AND op = 2)",
        params![commit, layer_id],
    )?;
    connection.execute(
        "INSERT OR REPLACE INTO main._lithograph_cp_properties(commit_id, owner_kind, owner_id, key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value) SELECT ?1, owner_kind, owner_id, key_id, type_tag, int_value, real_value, text_value, blob_value, aux_value FROM main._lithograph_property_delta WHERE layer_id = ?2 AND op = 1",
        params![commit, layer_id],
    )?;
    Ok(())
}

fn checkpoint_statistics(
    connection: &Connection,
    commit: HashId,
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
        )?,
        type_counts: grouped_counts(
            connection,
            "SELECT type_id, count(*) FROM main._lithograph_cp_relationships WHERE commit_id = ?1 GROUP BY type_id",
            commit,
        )?,
    })
}

fn grouped_counts(
    connection: &Connection,
    sql: &str,
    commit: &[u8],
) -> StorageResult<BTreeMap<i64, u64>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([commit], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    let mut counts = BTreeMap::new();
    for row in rows {
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
