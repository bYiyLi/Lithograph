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
        properties.insert((owner, owner_id, key), value);
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
    layer_between_checked(before, after, &|| Ok(()))
}

pub(crate) fn layer_between_checked(
    before: &SnapshotState,
    after: &SnapshotState,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<LayerBuilder> {
    check()?;
    let mut layer = LayerBuilder::default();
    append_node_label_delta(&mut layer, before, after, check)?;
    append_relationship_delta(&mut layer, before, after, check)?;
    append_property_delta(&mut layer, before, after, check)?;
    check()?;
    Ok(layer)
}

/// Collects the latest touched logical slots from `base` to a first-parent
/// descendant without resolving the base Snapshot.
pub(crate) fn touched_layer_between_commits(
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

    Ok(latest)
}

/// Computes the net Layer from `base` to a first-parent descendant without
/// materializing either complete Snapshot into Rust memory.
pub fn layer_between_commits(
    connection: &Connection,
    base: HashId,
    descendant: HashId,
) -> StorageResult<LayerBuilder> {
    let latest = touched_layer_between_commits(connection, base, descendant)?;
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
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    visit_set_changes(&before.nodes, &after.nodes, check, |node, added| {
        if added {
            layer.add_node(*node)
        } else {
            layer.remove_node(*node)
        }
    })?;
    visit_set_changes(
        &before.labels,
        &after.labels,
        check,
        |(node, label), added| {
            if added {
                layer.add_label(*node, *label)
            } else {
                layer.remove_label(*node, *label)
            }
        },
    )
}

fn visit_set_changes<T: Ord>(
    before: &BTreeSet<T>,
    after: &BTreeSet<T>,
    check: &dyn Fn() -> StorageResult<()>,
    mut visitor: impl FnMut(&T, bool) -> StorageResult<()>,
) -> StorageResult<()> {
    let mut before = before.iter().peekable();
    let mut after = after.iter().peekable();
    // Check equal slots too: std::collections::Difference::next can otherwise
    // scan an arbitrarily large unchanged prefix without yielding an element.
    while let (Some(left), Some(right)) = (before.peek().copied(), after.peek().copied()) {
        check()?;
        match left.cmp(right) {
            std::cmp::Ordering::Less => {
                visitor(left, false)?;
                before.next();
            }
            std::cmp::Ordering::Greater => {
                visitor(right, true)?;
                after.next();
            }
            std::cmp::Ordering::Equal => {
                before.next();
                after.next();
            }
        }
    }
    for value in before {
        check()?;
        visitor(value, false)?;
    }
    for value in after {
        check()?;
        visitor(value, true)?;
    }
    Ok(())
}

fn append_relationship_delta(
    layer: &mut LayerBuilder,
    before: &SnapshotState,
    after: &SnapshotState,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    for (id, relationship) in &before.relationships {
        check()?;
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
        check()?;
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
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    for (slot, before_value) in &before.properties {
        check()?;
        match after.properties.get(slot) {
            None => layer.remove_property(slot.0, slot.1, slot.2)?,
            Some(after_value) if property_changed_checked(before_value, after_value, check)? => {
                layer.set_property(
                    slot.0,
                    slot.1,
                    slot.2,
                    super::super::clone_property_value_checked(after_value, check)?,
                )?;
            }
            Some(_) => {}
        }
    }
    for (slot, value) in &after.properties {
        check()?;
        if !before.properties.contains_key(slot) {
            layer.set_property(
                slot.0,
                slot.1,
                slot.2,
                super::super::clone_property_value_checked(value, check)?,
            )?;
        }
    }
    Ok(())
}

fn property_changed_checked(
    before: &PropertyValue,
    after: &PropertyValue,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<bool> {
    let before = before.canonical_bytes_checked(check)?;
    let after = after.canonical_bytes_checked(check)?;
    Ok(!super::super::encoding::equal_bytes_checked(
        &before, &after, check,
    )?)
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;

    fn empty_state() -> SnapshotState {
        SnapshotState {
            nodes: BTreeSet::new(),
            labels: BTreeSet::new(),
            relationships: BTreeMap::new(),
            properties: BTreeMap::new(),
            schema: SchemaState::default(),
        }
    }

    #[test]
    fn checked_layer_diff_preserves_canonical_slot_order_and_signed_zero_changes() {
        let mut before = empty_state();
        before.nodes.extend([1, 2]);
        before.labels.insert((1, 1));
        let removed = RelationshipRecord {
            id: 1,
            source: 1,
            target: 2,
            type_id: 1,
        };
        before.relationships.insert(removed.id, removed);
        before.properties.insert(
            (OwnerKind::Node, 1, 1),
            PropertyValue::String("old".to_owned()),
        );
        before
            .properties
            .insert((OwnerKind::Node, 2, 1), PropertyValue::Float(-0.0));
        let mut after = empty_state();
        after.nodes.extend([2, 3]);
        after.labels.insert((3, 1));
        let added = RelationshipRecord {
            id: 2,
            source: 2,
            target: 3,
            type_id: 1,
        };
        after.relationships.insert(added.id, added);
        after
            .properties
            .insert((OwnerKind::Node, 2, 1), PropertyValue::Float(0.0));
        after.properties.insert(
            (OwnerKind::Node, 3, 1),
            PropertyValue::List(vec![PropertyValue::Integer(3)]),
        );
        let mut expected = LayerBuilder::default();
        expected.remove_node(1).unwrap();
        expected.add_node(3).unwrap();
        expected.remove_label(1, 1).unwrap();
        expected.add_label(3, 1).unwrap();
        expected.remove_relationship(removed).unwrap();
        expected.add_relationship(added).unwrap();
        expected.remove_property(OwnerKind::Node, 1, 1).unwrap();
        expected
            .set_property(OwnerKind::Node, 2, 1, PropertyValue::Float(0.0))
            .unwrap();
        expected
            .set_property(
                OwnerKind::Node,
                3,
                1,
                PropertyValue::List(vec![PropertyValue::Integer(3)]),
            )
            .unwrap();
        let actual = layer_between_checked(&before, &after, &|| Ok(())).unwrap();
        assert_eq!(
            actual.canonical_lce1().unwrap(),
            expected.canonical_lce1().unwrap()
        );
    }

    #[test]
    fn unchanged_set_prefix_still_observes_diff_cancellation() {
        let mut before = empty_state();
        before.nodes.extend(1..10_000);
        let checks = Cell::new(0);
        let error = layer_between_checked(&before, &before, &|| {
            checks.set(checks.get() + 1);
            if checks.get() == 257 {
                Err(StorageError::not_found("cancel unchanged prefix"))
            } else {
                Ok(())
            }
        })
        .unwrap_err();
        assert!(matches!(error, StorageError::NotFound(_)));
        assert_eq!(checks.get(), 257);
    }

    #[test]
    fn large_property_comparison_and_added_value_copy_observe_cancellation() {
        for compare_existing in [false, true] {
            let mut before = empty_state();
            let mut after = empty_state();
            if compare_existing {
                before.properties.insert(
                    (OwnerKind::Node, 1, 1),
                    PropertyValue::List(vec![PropertyValue::Integer(1); 2_000]),
                );
            }
            after.properties.insert(
                (OwnerKind::Node, 1, 1),
                PropertyValue::List(vec![PropertyValue::Integer(2); 2_000]),
            );
            let checks = Cell::new(0);
            let result = layer_between_checked(&before, &after, &|| {
                checks.set(checks.get() + 1);
                if checks.get() == 257 {
                    Err(StorageError::not_found("cancel property diff"))
                } else {
                    Ok(())
                }
            });
            assert!(matches!(result, Err(StorageError::NotFound(_))));
            assert_eq!(checks.get(), 257);
        }
    }
}
