//! First-parent lineage resolution for Snapshot construction.

use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension, Statement};

use super::{HashId, LayerBuilder, StorageError, StorageResult};

pub(super) fn resolve_lineage(
    connection: &Connection,
    commit: HashId,
    skip_checkpoint: Option<HashId>,
) -> StorageResult<(Option<HashId>, Vec<i64>)> {
    let mut current = commit;
    let mut layers = Vec::new();
    let mut visited = BTreeSet::new();
    let empty_layer_hash = LayerBuilder::default().content_hash()?;
    let mut statement = connection.prepare(
        "SELECT commits.parent1, commits.layer_id, layers.hash, \
                EXISTS(SELECT 1 FROM main._lithograph_checkpoints AS checkpoints \
                       WHERE checkpoints.commit_id = commits.id), \
                (EXISTS(SELECT 1 FROM main._lithograph_node_delta AS node_delta \
                        WHERE node_delta.layer_id = commits.layer_id) \
                 OR EXISTS(SELECT 1 FROM main._lithograph_label_delta AS label_delta \
                           WHERE label_delta.layer_id = commits.layer_id) \
                 OR EXISTS(SELECT 1 FROM main._lithograph_rel_delta AS rel_delta \
                           WHERE rel_delta.layer_id = commits.layer_id) \
                 OR EXISTS(SELECT 1 FROM main._lithograph_property_delta AS property_delta \
                           WHERE property_delta.layer_id = commits.layer_id)) \
         FROM main._lithograph_commits AS commits \
         JOIN main._lithograph_layers AS layers ON layers.id = commits.layer_id \
         WHERE commits.id = ?1",
    )?;
    loop {
        if !visited.insert(current) {
            return Err(StorageError::corrupt(
                "Commit first-parent lineage contains a cycle",
            ));
        }
        let row = load_lineage_row(&mut statement, current)?;
        if Some(current) != skip_checkpoint && row.has_checkpoint {
            return Ok((Some(current), layers));
        }
        record_lineage_layer(&row, empty_layer_hash, &mut layers)?;
        let Some(parent) = row.parent else {
            return Ok((None, layers));
        };
        current = parent;
    }
}

struct LineageRow {
    parent: Option<HashId>,
    layer_id: i64,
    layer_hash: HashId,
    has_checkpoint: bool,
    has_delta: bool,
}

fn load_lineage_row(statement: &mut Statement<'_>, current: HashId) -> StorageResult<LineageRow> {
    let row = statement
        .query_row([current.as_bytes().as_slice()], |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Vec<u8>>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })
        .optional()?
        .ok_or_else(|| StorageError::not_found(format!("Commit {}", current.to_hex())))?;
    Ok(LineageRow {
        parent: row.0.as_deref().map(HashId::from_slice).transpose()?,
        layer_id: row.1,
        layer_hash: HashId::from_slice(&row.2)?,
        has_checkpoint: row.3 == 1,
        has_delta: row.4 == 1,
    })
}

fn record_lineage_layer(
    row: &LineageRow,
    empty_layer_hash: HashId,
    layers: &mut Vec<i64>,
) -> StorageResult<()> {
    if row.has_delta {
        layers.push(row.layer_id);
        return Ok(());
    }
    if row.layer_hash != empty_layer_hash {
        return Err(StorageError::corrupt(format!(
            "empty Layer {} has a non-canonical content hash",
            row.layer_id
        )));
    }
    Ok(())
}
