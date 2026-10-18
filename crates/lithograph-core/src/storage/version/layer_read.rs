//! Allocation-controlled sparse first-parent Layer resolution.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Debug;
use std::rc::Rc;

use rusqlite::Connection;

use crate::storage::layer::{DeltaOp, LayerBuilder, PropertyDelta, RelationshipDelta};
use crate::storage::layer_read::{
    property_delta_from_row_observed, visit_label_deltas, visit_node_deltas,
    visit_relationship_deltas,
};
use crate::storage::snapshot_read::{self, ReadLease, SnapshotReadObserver};
use crate::storage::{HashId, OwnerKind, PropertyValue, Snapshot, StorageError, StorageResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum Slot {
    Node(i64),
    Label(i64, i64),
    Relationship(i64),
    Property(OwnerKind, i64, i64),
}

type Leases = BTreeMap<Slot, ReadLease>;

struct LayerReader<'a> {
    connection: &'a Connection,
    observer: &'a dyn SnapshotReadObserver,
    check: &'a dyn Fn() -> StorageResult<()>,
}

pub(crate) fn touched_layer_between_commits_observed(
    connection: &Connection,
    base: HashId,
    descendant: HashId,
    observer: Rc<dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<(LayerBuilder, Rc<dyn Debug>)> {
    let (layer, memory) = load_touched(connection, base, descendant, observer.as_ref(), check)?;
    Ok((layer, Rc::new(memory)))
}

pub(crate) fn layer_between_commits_observed(
    connection: &Connection,
    base: HashId,
    descendant: HashId,
    observer: Rc<dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<(LayerBuilder, Rc<dyn Debug>)> {
    if base == descendant {
        return Ok((LayerBuilder::default(), Rc::new(Vec::<ReadLease>::new())));
    }
    let (latest, mut memory) =
        load_touched(connection, base, descendant, observer.as_ref(), check)?;
    let snapshot = Snapshot::resolve_observed(connection, base, observer.clone(), check)?;
    let mut result = LayerBuilder::default();
    normalize_nodes(&snapshot, &mut result, latest.nodes, check)?;
    normalize_labels(
        &snapshot,
        &mut result,
        latest.labels,
        observer.as_ref(),
        check,
    )?;
    normalize_relationships(&snapshot, &mut result, latest.relationships, check)?;
    normalize_properties(
        &snapshot,
        &mut result,
        latest.properties,
        &mut memory,
        observer.as_ref(),
        check,
    )?;
    let mut retained = Vec::new();
    for (slot, memory) in memory {
        check()?;
        if layer_contains(&result, slot) {
            retained.push(memory);
        }
    }
    Ok((result, Rc::new(retained)))
}

fn load_touched(
    connection: &Connection,
    base: HashId,
    mut current: HashId,
    observer: &dyn SnapshotReadObserver,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<(LayerBuilder, Leases)> {
    let reader = LayerReader {
        connection,
        observer,
        check,
    };
    let mut latest = LayerBuilder::default();
    let mut memory = BTreeMap::new();
    while current != base {
        check()?;
        let (parent, layer): (Option<Vec<u8>>, i64) = connection.query_row(
            "SELECT parent1, layer_id FROM main._lithograph_commits WHERE id=?1",
            [current.as_bytes().as_slice()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        reader.append_metadata(layer, &mut latest, &mut memory)?;
        reader.append_properties(layer, &mut latest, &mut memory)?;
        current = parent
            .as_deref()
            .map(HashId::from_slice)
            .transpose()?
            .ok_or_else(|| {
                StorageError::corrupt(
                    "Commit is not a first-parent descendant of the requested base",
                )
            })?;
    }
    Ok((latest, memory))
}

impl LayerReader<'_> {
    fn append_metadata(
        &self,
        layer: i64,
        latest: &mut LayerBuilder,
        memory: &mut Leases,
    ) -> StorageResult<()> {
        visit_node_deltas(self.connection, layer, |id, op| {
            (self.check)()?;
            if let std::collections::btree_map::Entry::Vacant(entry) = latest.nodes.entry(id) {
                retain(memory, Slot::Node(id), self.observer)?;
                entry.insert(op);
            }
            Ok(())
        })?;
        visit_label_deltas(self.connection, layer, |id, label, op| {
            (self.check)()?;
            if let std::collections::btree_map::Entry::Vacant(entry) =
                latest.labels.entry((id, label))
            {
                retain(memory, Slot::Label(id, label), self.observer)?;
                entry.insert(op);
            }
            Ok(())
        })?;
        visit_relationship_deltas(self.connection, layer, |record, op| {
            (self.check)()?;
            if let std::collections::btree_map::Entry::Vacant(entry) =
                latest.relationships.entry(record.id)
            {
                retain(memory, Slot::Relationship(record.id), self.observer)?;
                entry.insert(RelationshipDelta { op, record });
            }
            Ok(())
        })
    }

    fn append_properties(
        &self,
        layer: i64,
        latest: &mut LayerBuilder,
        memory: &mut Leases,
    ) -> StorageResult<()> {
        let mut statement = self.connection.prepare("SELECT owner_kind, owner_id, key_id, op, type_tag, int_value, real_value, text_value, blob_value, aux_value FROM main._lithograph_property_delta WHERE layer_id=?1 ORDER BY owner_kind, owner_id, key_id")?;
        let mut rows = statement.query([layer])?;
        while let Some(row) = rows.next()? {
            (self.check)()?;
            let key = (
                OwnerKind::from_i64(row.get(0)?)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            );
            if latest.properties.contains_key(&key) {
                continue;
            }
            let (delta, lease) =
                property_delta_from_row_observed(row, Some(self.observer), self.check)?;
            let slot = Slot::Property(key.0, key.1, key.2);
            match lease {
                Some(lease) => {
                    memory.insert(slot, lease);
                }
                None => retain(memory, slot, self.observer)?,
            }
            latest.properties.insert(key, delta);
        }
        Ok(())
    }
}

fn normalize_nodes(
    snapshot: &Snapshot<'_>,
    result: &mut LayerBuilder,
    nodes: BTreeMap<i64, DeltaOp>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    for (id, op) in nodes {
        check()?;
        match (snapshot.node_exists(id)?, op == DeltaOp::Add) {
            (false, true) => result.add_node(id)?,
            (true, false) => result.remove_node(id)?,
            _ => {}
        }
    }
    Ok(())
}

fn normalize_labels(
    snapshot: &Snapshot<'_>,
    result: &mut LayerBuilder,
    deltas: BTreeMap<(i64, i64), DeltaOp>,
    observer: &dyn SnapshotReadObserver,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    let mut owner = None;
    let mut labels = BTreeSet::new();
    let mut memory = Vec::new();
    for ((id, label), op) in deltas {
        check()?;
        if owner != Some(id) {
            labels.clear();
            memory.clear();
            snapshot.visit_node_labels_checked(id, check, |label| {
                if let Some(lease) = snapshot_read::reserve(Some(observer), 256)? {
                    memory.push(lease);
                }
                labels.insert(label);
                Ok::<_, StorageError>(())
            })?;
            owner = Some(id);
        }
        match (labels.contains(&label), op == DeltaOp::Add) {
            (false, true) => result.add_label(id, label)?,
            (true, false) => result.remove_label(id, label)?,
            _ => {}
        }
    }
    Ok(())
}

fn normalize_relationships(
    snapshot: &Snapshot<'_>,
    result: &mut LayerBuilder,
    relationships: BTreeMap<i64, RelationshipDelta>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    for (id, delta) in relationships {
        check()?;
        match (
            snapshot.relationship(id)?,
            (delta.op == DeltaOp::Add).then_some(delta.record),
        ) {
            (None, Some(after)) => result.add_relationship(after)?,
            (Some(before), None) => result.remove_relationship(before)?,
            (Some(before), Some(after)) if before != after => {
                return Err(StorageError::corrupt(
                    "Relationship changed immutable endpoints or type",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

fn normalize_properties(
    snapshot: &Snapshot<'_>,
    result: &mut LayerBuilder,
    properties: BTreeMap<(OwnerKind, i64, i64), PropertyDelta>,
    memory: &mut Leases,
    observer: &dyn SnapshotReadObserver,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    for ((kind, id, key), delta) in properties {
        check()?;
        let changed = snapshot.with_property_checked(kind, id, key, check, |before| {
            property_changed(before, delta.value.as_ref(), observer, check)
        })?;
        if changed {
            match delta.value {
                Some(value) => result.set_property(kind, id, key, value)?,
                None => result.remove_property(kind, id, key)?,
            }
        } else {
            drop(delta);
            memory.remove(&Slot::Property(kind, id, key));
        }
    }
    Ok(())
}

fn property_changed(
    before: Option<&PropertyValue>,
    after: Option<&PropertyValue>,
    observer: &dyn SnapshotReadObserver,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<bool> {
    let (Some(before), Some(after)) = (before, after) else {
        return Ok(before.is_some() != after.is_some());
    };
    let bytes = snapshot_read::checked_add(
        crate::storage::property_value_memory_bytes(before, check)?,
        crate::storage::property_value_memory_bytes(after, check)?,
    )?;
    let bytes = bytes
        .checked_mul(4)
        .ok_or_else(|| StorageError::corrupt("property comparison size overflow"))?;
    let _temporary = snapshot_read::reserve(Some(observer), bytes)?;
    Ok(before.canonical_bytes_checked(check)? != after.canonical_bytes_checked(check)?)
}

fn retain(
    memory: &mut Leases,
    slot: Slot,
    observer: &dyn SnapshotReadObserver,
) -> StorageResult<()> {
    if let Some(lease) = snapshot_read::reserve(Some(observer), 256)? {
        memory.insert(slot, lease);
    }
    Ok(())
}

fn layer_contains(layer: &LayerBuilder, slot: Slot) -> bool {
    match slot {
        Slot::Node(id) => layer.nodes.contains_key(&id),
        Slot::Label(id, label) => layer.labels.contains_key(&(id, label)),
        Slot::Relationship(id) => layer.relationships.contains_key(&id),
        Slot::Property(kind, id, key) => layer.properties.contains_key(&(kind, id, key)),
    }
}
