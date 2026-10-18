//! Applying canonical Layer rows to a query-local Snapshot overlay.

use rusqlite::Connection;

use super::StorageResult;
use super::layer::{LayerBuilder, RelationshipDelta, RelationshipRecord};
use super::layer_read::{
    visit_label_deltas, visit_node_deltas, visit_property_deltas_observed,
    visit_relationship_deltas,
};
use super::snapshot::Overlay;
use super::snapshot_read::{self, SnapshotReadObserver};
use super::snapshot_staged::{LocalSlot, overlay_slot_bytes};

impl Overlay {
    pub(super) fn apply(&mut self, layer: LayerBuilder) {
        super::snapshot_staged::insert_entries(&mut self.nodes, layer.nodes);
        for ((node_id, label_id), op) in layer.labels {
            self.labels.insert((node_id, label_id), op);
            self.labels_by_label.insert((label_id, node_id), op);
        }
        for (relationship_id, delta) in layer.relationships {
            self.apply_relationship_delta(relationship_id, delta);
        }
        super::snapshot_staged::insert_entries(&mut self.properties, layer.properties);
    }

    pub(super) fn clone_observed(
        &self,
        observer: Option<&dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Self> {
        let mut output = Self::default();
        for (id, op) in &self.nodes {
            output.insert_node_observed(*id, *op, observer, check)?;
        }
        for ((id, label), op) in &self.labels {
            output.insert_label_observed(*id, *label, *op, observer, check)?;
        }
        for (id, delta) in &self.relationships {
            output.insert_relationship_observed(*id, *delta, observer, check)?;
        }
        for (key, delta) in &self.properties {
            output.clone_property_observed(*key, delta, observer, check)?;
        }
        Ok(output)
    }

    pub(super) fn apply_layer_observed(
        &mut self,
        layer: &LayerBuilder,
        observer: Option<&dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        for (id, op) in &layer.nodes {
            self.insert_node_observed(*id, *op, observer, check)?;
        }
        for ((id, label), op) in &layer.labels {
            self.insert_label_observed(*id, *label, *op, observer, check)?;
        }
        for (id, delta) in &layer.relationships {
            self.insert_relationship_observed(*id, *delta, observer, check)?;
        }
        for (key, delta) in &layer.properties {
            self.clone_property_observed(*key, delta, observer, check)?;
        }
        Ok(())
    }

    fn clone_property_observed(
        &mut self,
        key: (super::OwnerKind, i64, i64),
        delta: &super::layer::PropertyDelta,
        observer: Option<&dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        let memory = delta
            .value
            .as_ref()
            .map(|value| snapshot_read::reserve_value(value, observer, check))
            .transpose()?
            .flatten();
        let delta = super::layer::PropertyDelta {
            op: delta.op,
            value: delta
                .value
                .as_ref()
                .map(|value| snapshot_read::clone_value_checked(value, check))
                .transpose()?,
        };
        self.insert_property_observed(key, delta, memory, observer)
    }

    fn insert_property_observed(
        &mut self,
        key: (super::OwnerKind, i64, i64),
        delta: super::layer::PropertyDelta,
        memory: Option<snapshot_read::ReadLease>,
        observer: Option<&dyn SnapshotReadObserver>,
    ) -> StorageResult<()> {
        // Data and lease maps each allocate a whole B-tree node for their first
        // entry. Value decoding's lease covers the payload, not these trees.
        let structure = snapshot_read::reserve(
            observer,
            snapshot_read::checked_add(
                overlay_slot_bytes(LocalSlot::Property(key.0, key.1, key.2))?,
                256,
            )?,
        )?;
        let memory = match (memory, structure) {
            (None, None) => None,
            memory => Some(std::rc::Rc::new(memory) as snapshot_read::ReadLease),
        };
        self.properties.insert(key, delta);
        if let Some(memory) = memory {
            self.property_memory.insert(key, memory);
        } else {
            self.property_memory.remove(&key);
        }
        Ok(())
    }

    fn retain_structure(
        &mut self,
        observer: Option<&dyn SnapshotReadObserver>,
        bytes: u64,
    ) -> StorageResult<()> {
        if let Some(memory) = snapshot_read::reserve(observer, bytes)? {
            self.structure_memory.push(memory);
        }
        Ok(())
    }

    fn insert_node_observed(
        &mut self,
        id: i64,
        op: super::layer::DeltaOp,
        observer: Option<&dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        check()?;
        if !self.nodes.contains_key(&id) {
            self.retain_structure(
                observer,
                snapshot_read::checked_add(overlay_slot_bytes(LocalSlot::Node(id))?, 256)?,
            )?;
        }
        self.nodes.insert(id, op);
        Ok(())
    }

    fn insert_label_observed(
        &mut self,
        id: i64,
        label: i64,
        op: super::layer::DeltaOp,
        observer: Option<&dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        check()?;
        if !self.labels.contains_key(&(id, label)) {
            self.retain_structure(
                observer,
                snapshot_read::checked_add(overlay_slot_bytes(LocalSlot::Label(id, label))?, 256)?,
            )?;
        }
        self.labels.insert((id, label), op);
        self.labels_by_label.insert((label, id), op);
        Ok(())
    }

    fn insert_relationship_observed(
        &mut self,
        id: i64,
        delta: RelationshipDelta,
        observer: Option<&dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        check()?;
        if !self.relationships.contains_key(&id) {
            self.retain_structure(
                observer,
                snapshot_read::checked_add(overlay_slot_bytes(LocalSlot::Relationship(id))?, 256)?,
            )?;
        }
        self.apply_relationship_delta(id, delta);
        Ok(())
    }

    pub(super) fn apply_persisted_layer_observed(
        &mut self,
        connection: &Connection,
        layer_id: i64,
        observer: Option<&dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        visit_node_deltas(connection, layer_id, |id, op| {
            self.insert_node_observed(id, op, observer, check)
        })?;
        visit_label_deltas(connection, layer_id, |id, label, op| {
            self.insert_label_observed(id, label, op, observer, check)
        })?;
        visit_relationship_deltas(connection, layer_id, |record, op| {
            self.insert_relationship_observed(
                record.id,
                RelationshipDelta { op, record },
                observer,
                check,
            )
        })?;
        visit_property_deltas_observed(
            connection,
            layer_id,
            observer,
            check,
            |kind, owner, key, delta, memory| {
                self.insert_property_observed((kind, owner, key), delta, memory, observer)
            },
        )?;
        Ok(())
    }

    fn apply_relationship_delta(&mut self, relationship_id: i64, delta: RelationshipDelta) {
        if let Some(previous) = self.relationships.get(&relationship_id) {
            self.outgoing.remove(&outgoing_key(previous.record));
            self.incoming.remove(&incoming_key(previous.record));
            self.outgoing_by_id
                .remove(&(previous.record.source, relationship_id));
            self.incoming_by_id
                .remove(&(previous.record.target, relationship_id));
            self.incident_by_id
                .remove(&(previous.record.source, relationship_id));
            self.incident_by_id
                .remove(&(previous.record.target, relationship_id));
        }
        self.outgoing.insert(outgoing_key(delta.record), delta);
        self.incoming.insert(incoming_key(delta.record), delta);
        self.outgoing_by_id
            .insert((delta.record.source, relationship_id), delta);
        self.incoming_by_id
            .insert((delta.record.target, relationship_id), delta);
        self.incident_by_id
            .insert((delta.record.source, relationship_id), delta);
        self.incident_by_id
            .insert((delta.record.target, relationship_id), delta);
        self.relationships.insert(relationship_id, delta);
    }
}

fn outgoing_key(record: RelationshipRecord) -> (i64, i64, i64, i64) {
    (record.source, record.type_id, record.target, record.id)
}

fn incoming_key(record: RelationshipRecord) -> (i64, i64, i64, i64) {
    (record.target, record.type_id, record.source, record.id)
}
