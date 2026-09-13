use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::FromSql;
use rusqlite::{Connection, params};

use super::super::{
    HashId, LayerBuilder, NodeId, OwnerKind, PropertyKeyId, PropertyValue, RelationshipId,
    RelationshipRecord, SchemaState, Snapshot, StorageError, StorageResult,
};

#[derive(Debug, Clone, PartialEq)]
pub struct SnapshotState {
    pub nodes: BTreeSet<NodeId>,
    pub labels: BTreeSet<(NodeId, i64)>,
    pub relationships: BTreeMap<RelationshipId, RelationshipRecord>,
    pub properties: BTreeMap<(OwnerKind, i64, PropertyKeyId), PropertyValue>,
    pub schema: SchemaState,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AllocationState {
    pub sequences: Vec<(i64, i64)>,
    pub labels: Vec<(i64, String)>,
    pub relationship_types: Vec<(i64, String)>,
    pub property_keys: Vec<(i64, String)>,
}

pub fn load_snapshot_state(
    connection: &Connection,
    commit: HashId,
) -> StorageResult<SnapshotState> {
    let snapshot = Snapshot::resolve(connection, commit)?;
    let mut nodes = BTreeSet::new();
    snapshot.visit_nodes(|node| {
        nodes.insert(node);
        Ok(())
    })?;
    let mut labels = BTreeSet::new();
    snapshot.visit_labels(|node, label| {
        labels.insert((node, label));
        Ok(())
    })?;
    let mut relationships = BTreeMap::new();
    snapshot.visit_relationships(|relationship| {
        relationships.insert(relationship.id, relationship);
        Ok(())
    })?;
    let mut properties = BTreeMap::new();
    snapshot.visit_properties(|owner, owner_id, key, value| {
        properties.insert((owner, owner_id, key), value.clone());
        Ok(())
    })?;
    Ok(SnapshotState {
        nodes,
        labels,
        relationships,
        properties,
        schema: SchemaState::load(connection, commit)?,
    })
}

pub fn layer_between(before: &SnapshotState, after: &SnapshotState) -> StorageResult<LayerBuilder> {
    let mut layer = LayerBuilder::default();
    append_node_label_delta(&mut layer, before, after)?;
    append_relationship_delta(&mut layer, before, after)?;
    append_property_delta(&mut layer, before, after)?;
    Ok(layer)
}

fn append_node_label_delta(
    layer: &mut LayerBuilder,
    before: &SnapshotState,
    after: &SnapshotState,
) -> StorageResult<()> {
    for node in before.nodes.difference(&after.nodes) {
        layer.remove_node(*node)?;
    }
    for node in after.nodes.difference(&before.nodes) {
        layer.add_node(*node)?;
    }
    for (node, label) in before.labels.difference(&after.labels) {
        layer.remove_label(*node, *label)?;
    }
    for (node, label) in after.labels.difference(&before.labels) {
        layer.add_label(*node, *label)?;
    }
    Ok(())
}

fn append_relationship_delta(
    layer: &mut LayerBuilder,
    before: &SnapshotState,
    after: &SnapshotState,
) -> StorageResult<()> {
    for (id, relationship) in &before.relationships {
        match after.relationships.get(id) {
            None => layer.remove_relationship(*relationship)?,
            Some(after_relationship) if after_relationship != relationship => {
                return Err(StorageError::corrupt(format!(
                    "RelationshipId {id} changed immutable endpoints or type"
                )));
            }
            Some(_) => {}
        }
    }
    for (id, relationship) in &after.relationships {
        if !before.relationships.contains_key(id) {
            layer.add_relationship(*relationship)?;
        }
    }
    Ok(())
}

fn append_property_delta(
    layer: &mut LayerBuilder,
    before: &SnapshotState,
    after: &SnapshotState,
) -> StorageResult<()> {
    for (slot, before_value) in &before.properties {
        match after.properties.get(slot) {
            None => layer.remove_property(slot.0, slot.1, slot.2)?,
            Some(after_value) if after_value != before_value => {
                layer.set_property(slot.0, slot.1, slot.2, after_value.clone())?;
            }
            Some(_) => {}
        }
    }
    for (slot, value) in &after.properties {
        if !before.properties.contains_key(slot) {
            layer.set_property(slot.0, slot.1, slot.2, value.clone())?;
        }
    }
    Ok(())
}

pub fn capture_allocation_state(connection: &Connection) -> StorageResult<AllocationState> {
    Ok(AllocationState {
        sequences: read_integer_pairs(
            connection,
            "SELECT kind, next_id FROM main._lithograph_sequences ORDER BY kind",
        )?,
        labels: read_dictionary(connection, "_lithograph_labels")?,
        relationship_types: read_dictionary(connection, "_lithograph_rel_types")?,
        property_keys: read_dictionary(connection, "_lithograph_prop_keys")?,
    })
}

pub fn restore_allocation_state(
    connection: &Connection,
    state: &AllocationState,
) -> StorageResult<()> {
    restore_dictionary(connection, "_lithograph_labels", &state.labels)?;
    restore_dictionary(
        connection,
        "_lithograph_rel_types",
        &state.relationship_types,
    )?;
    restore_dictionary(connection, "_lithograph_prop_keys", &state.property_keys)?;
    for (kind, next_id) in &state.sequences {
        connection.execute(
            "UPDATE main._lithograph_sequences SET next_id = max(next_id, ?2) WHERE kind = ?1",
            params![kind, next_id],
        )?;
    }
    Ok(())
}

fn read_integer_pairs(connection: &Connection, sql: &str) -> StorageResult<Vec<(i64, i64)>> {
    read_pairs(connection, sql)
}

fn read_dictionary(connection: &Connection, table: &str) -> StorageResult<Vec<(i64, String)>> {
    let sql = format!("SELECT id, name FROM main.{table} ORDER BY id");
    read_pairs(connection, &sql)
}

fn read_pairs<A: FromSql, B: FromSql>(
    connection: &Connection,
    sql: &str,
) -> StorageResult<Vec<(A, B)>> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from)
}

fn restore_dictionary(
    connection: &Connection,
    table: &str,
    values: &[(i64, String)],
) -> StorageResult<()> {
    let sql = format!("INSERT OR IGNORE INTO main.{table}(id, name) VALUES(?1, ?2)");
    let mut statement = connection.prepare(&sql)?;
    for (id, name) in values {
        statement.execute(params![id, name])?;
    }
    Ok(())
}
