use std::collections::{BTreeMap, BTreeSet};

use rusqlite::types::FromSql;
use rusqlite::{Connection, params};

use super::super::layer::{DeltaOp, PropertyDelta, RelationshipDelta, load_layer};
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

/// Computes the net Layer from `base` to a first-parent descendant without
/// materializing either complete Snapshot into Rust memory.
///
/// Only logical slots touched by the descendant chain are retained. Each slot
/// is normalized against the base Snapshot through point lookups so create /
/// delete or remove / restore sequences that cancel within the chain disappear
/// from the resulting Layer.
pub fn layer_between_commits(
    connection: &Connection,
    base: HashId,
    descendant: HashId,
) -> StorageResult<LayerBuilder> {
    if base == descendant {
        return Ok(LayerBuilder::default());
    }
    let mut latest = LayerBuilder::default();
    let mut current = descendant;
    while current != base {
        let (parent, layer_id): (Option<Vec<u8>>, i64) = connection
            .query_row(
                "SELECT parent1, layer_id FROM main._lithograph_commits WHERE id = ?1",
                [current.as_bytes().as_slice()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(StorageError::from)?;
        let layer = load_layer(connection, layer_id)?;
        for (node_id, op) in layer.nodes {
            latest.nodes.entry(node_id).or_insert(op);
        }
        for (slot, op) in layer.labels {
            latest.labels.entry(slot).or_insert(op);
        }
        for (relationship_id, delta) in layer.relationships {
            latest.relationships.entry(relationship_id).or_insert(delta);
        }
        for (slot, delta) in layer.properties {
            latest.properties.entry(slot).or_insert(delta);
        }
        current = parent
            .as_deref()
            .map(HashId::from_slice)
            .transpose()?
            .ok_or_else(|| {
                StorageError::corrupt(format!(
                    "Commit {} is not a first-parent descendant of {}",
                    descendant.to_hex(),
                    base.to_hex()
                ))
            })?;
    }

    normalize_touched_layer(connection, base, latest)
}

pub fn is_first_parent_descendant(
    connection: &Connection,
    ancestor: HashId,
    mut descendant: HashId,
) -> StorageResult<bool> {
    while descendant != ancestor {
        let record = super::history::load_commit(connection, descendant)?;
        let Some(parent) = record.parent1 else {
            return Ok(false);
        };
        descendant = parent;
    }
    Ok(true)
}

fn normalize_touched_layer(
    connection: &Connection,
    base_commit: HashId,
    latest: LayerBuilder,
) -> StorageResult<LayerBuilder> {
    let base = Snapshot::resolve(connection, base_commit)?;
    let mut result = LayerBuilder::default();
    normalize_touched_nodes(&base, &mut result, latest.nodes)?;
    normalize_touched_labels(&base, &mut result, latest.labels)?;
    normalize_touched_relationships(&base, &mut result, latest.relationships)?;
    normalize_touched_properties(&base, &mut result, latest.properties)?;
    Ok(result)
}

fn normalize_touched_nodes(
    base: &Snapshot<'_>,
    result: &mut LayerBuilder,
    nodes: BTreeMap<NodeId, DeltaOp>,
) -> StorageResult<()> {
    for (node_id, op) in nodes {
        let before = base.node_exists(node_id)?;
        let after = op == DeltaOp::Add;
        match (before, after) {
            (false, true) => result.add_node(node_id)?,
            (true, false) => result.remove_node(node_id)?,
            _ => {}
        }
    }
    Ok(())
}

fn normalize_touched_labels(
    base: &Snapshot<'_>,
    result: &mut LayerBuilder,
    labels: BTreeMap<(NodeId, i64), DeltaOp>,
) -> StorageResult<()> {
    let mut base_labels = BTreeMap::<NodeId, BTreeSet<i64>>::new();
    for ((node_id, label_id), op) in labels {
        let labels = match base_labels.entry(node_id) {
            std::collections::btree_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert(base.labels(node_id)?.into_iter().collect())
            }
        };
        let before = labels.contains(&label_id);
        let after = op == DeltaOp::Add;
        match (before, after) {
            (false, true) => result.add_label(node_id, label_id)?,
            (true, false) => result.remove_label(node_id, label_id)?,
            _ => {}
        }
    }
    Ok(())
}

fn normalize_touched_relationships(
    base: &Snapshot<'_>,
    result: &mut LayerBuilder,
    relationships: BTreeMap<RelationshipId, RelationshipDelta>,
) -> StorageResult<()> {
    for (relationship_id, delta) in relationships {
        let before = base.relationship(relationship_id)?;
        let after = (delta.op == DeltaOp::Add).then_some(delta.record);
        match (before, after) {
            (None, Some(relationship)) => result.add_relationship(relationship)?,
            (Some(relationship), None) => result.remove_relationship(relationship)?,
            (Some(before), Some(after)) if before != after => {
                return Err(StorageError::corrupt(format!(
                    "RelationshipId {relationship_id} changed immutable endpoints or type"
                )));
            }
            _ => {}
        }
    }
    Ok(())
}

fn normalize_touched_properties(
    base: &Snapshot<'_>,
    result: &mut LayerBuilder,
    properties: BTreeMap<(OwnerKind, i64, PropertyKeyId), PropertyDelta>,
) -> StorageResult<()> {
    for ((owner, owner_id, key_id), delta) in properties {
        let before = base.property(owner, owner_id, key_id)?;
        let after = if delta.op == DeltaOp::Add {
            delta.value
        } else {
            None
        };
        match (before, after) {
            (None, Some(value)) => result.set_property(owner, owner_id, key_id, value)?,
            (Some(_), None) => result.remove_property(owner, owner_id, key_id)?,
            (Some(before), Some(after))
                if before.canonical_bytes()? != after.canonical_bytes()? =>
            {
                result.set_property(owner, owner_id, key_id, after)?;
            }
            _ => {}
        }
    }
    Ok(())
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
