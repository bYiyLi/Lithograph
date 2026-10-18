//! Allocation-controlled copy-on-write staged Snapshot state.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;

use super::layer::{DeltaOp, LayerBuilder, PropertyDelta, RelationshipDelta};
use super::snapshot::{Overlay, Snapshot, advance_query_local_cache_identity};
use super::snapshot_read::{self, ReadLease, SnapshotReadObserver};
use super::{OwnerKind, StorageError, StorageResult};

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum LocalSlot {
    Node(i64),
    Label(i64, i64),
    Relationship(i64),
    Property(OwnerKind, i64, i64),
}

/// These leases share exactly the lifetime of the corresponding local Layer.
/// A replacement transfers its new lease and releases the previous value's lease.
#[derive(Debug, Default)]
pub(super) struct LocalLayerMemory {
    slots: BTreeMap<LocalSlot, ReadLease>,
}

struct PreparedLayer {
    layer: LayerBuilder,
    memory: LocalLayerMemory,
}

impl Snapshot<'_> {
    pub(crate) fn apply_layer_checked(
        &mut self,
        layer: &LayerBuilder,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        let observer = self.state.read_observer.clone();
        let observer = observer.as_deref();
        // Prepare only the incoming changes. Full copies are needed solely when
        // another Snapshot still owns the old local Layer or overlay.
        let local_delta = prepare_layer(layer, observer, false, check)?;
        let overlay_delta = prepare_layer(layer, observer, true, check)?;
        let local_copy = self.prepare_shared_local(observer, check)?;
        let overlay_copy = if Rc::strong_count(&self.state.overlay) > 1 {
            Some(self.state.overlay.clone_observed(observer, check)?)
        } else {
            None
        };
        let hash_bytes = snapshot_read::layer_memory_bytes(layer, check)?
            .checked_mul(4)
            .ok_or_else(snapshot_read::resource_overflow)?;
        let hash_memory = snapshot_read::reserve(observer, hash_bytes)?;
        let layer_hash = layer.content_hash_checked(check)?;
        drop(hash_memory);
        check()?;

        // All fallible copies, budget reservations and cancellation checks finish
        // before publication. Publishing moves the already-owned map entries.
        self.publish_local(local_copy, local_delta)?;
        if let Some(overlay) = overlay_copy {
            self.state.overlay = Rc::new(overlay);
        }
        let overlay = Rc::get_mut(&mut self.state.overlay)
            .ok_or_else(|| StorageError::corrupt("staged overlay remained shared"))?;
        publish_overlay(overlay, overlay_delta);
        self.state.cache_identity =
            advance_query_local_cache_identity(self.state.cache_identity, layer_hash);
        Ok(())
    }

    fn prepare_shared_local(
        &self,
        observer: Option<&dyn SnapshotReadObserver>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Option<PreparedLayer>> {
        let Some(local) = &self.state.query_local_layer else {
            return Ok(None);
        };
        let shared_memory = self
            .state
            .local_memory
            .as_ref()
            .is_some_and(|memory| Rc::strong_count(memory) > 1);
        if Arc::strong_count(local) > 1 || shared_memory {
            prepare_layer(local, observer, false, check).map(Some)
        } else {
            Ok(None)
        }
    }

    fn publish_local(
        &mut self,
        copy: Option<PreparedLayer>,
        delta: PreparedLayer,
    ) -> StorageResult<()> {
        if let Some(copy) = copy {
            self.state.query_local_layer = Some(Arc::new(copy.layer));
            self.state.local_memory = Some(Rc::new(copy.memory));
        }
        let local = self
            .state
            .query_local_layer
            .get_or_insert_with(|| Arc::new(LayerBuilder::default()));
        let memory = self
            .state
            .local_memory
            .get_or_insert_with(|| Rc::new(LocalLayerMemory::default()));
        let local = Arc::get_mut(local)
            .ok_or_else(|| StorageError::corrupt("staged local Layer remained shared"))?;
        let memory = Rc::get_mut(memory)
            .ok_or_else(|| StorageError::corrupt("staged local leases remained shared"))?;
        // Insertion is O(changes * log(existing slots)); BTreeMap::append would
        // rebuild the accumulated map even for one incoming property.
        insert_entries(&mut local.nodes, delta.layer.nodes);
        insert_entries(&mut local.labels, delta.layer.labels);
        insert_entries(&mut local.relationships, delta.layer.relationships);
        insert_entries(&mut local.properties, delta.layer.properties);
        insert_entries(&mut memory.slots, delta.memory.slots);
        Ok(())
    }
}

pub(super) fn prepare_local_layer(
    layer: &LayerBuilder,
    observer: Option<&dyn SnapshotReadObserver>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<(LayerBuilder, Rc<LocalLayerMemory>)> {
    let prepared = prepare_layer(layer, observer, false, check)?;
    Ok((prepared.layer, Rc::new(prepared.memory)))
}

fn prepare_layer(
    layer: &LayerBuilder,
    observer: Option<&dyn SnapshotReadObserver>,
    overlay: bool,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PreparedLayer> {
    let mut memory = LocalLayerMemory::default();
    reserve_metadata(&mut memory, layer, observer, overlay, check)?;
    for ((owner, id, key), delta) in &layer.properties {
        check()?;
        let bytes = match &delta.value {
            Some(value) => snapshot_read::checked_add(
                super::read_memory::property_value_bytes(value, check)?,
                slot_bytes(LocalSlot::Property(*owner, *id, *key), overlay)?,
            )?,
            None => slot_bytes(LocalSlot::Property(*owner, *id, *key), overlay)?,
        };
        memory.reserve(LocalSlot::Property(*owner, *id, *key), bytes, observer)?;
    }
    Ok(PreparedLayer {
        layer: snapshot_read::clone_layer_checked(layer, check)?,
        memory,
    })
}

fn reserve_metadata(
    memory: &mut LocalLayerMemory,
    layer: &LayerBuilder,
    observer: Option<&dyn SnapshotReadObserver>,
    overlay: bool,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<()> {
    for key in layer.nodes.keys() {
        check()?;
        memory.reserve(
            LocalSlot::Node(*key),
            slot_bytes(LocalSlot::Node(*key), overlay)?,
            observer,
        )?;
    }
    for (node, label) in layer.labels.keys() {
        check()?;
        memory.reserve(
            LocalSlot::Label(*node, *label),
            slot_bytes(LocalSlot::Label(*node, *label), overlay)?,
            observer,
        )?;
    }
    for key in layer.relationships.keys() {
        check()?;
        memory.reserve(
            LocalSlot::Relationship(*key),
            slot_bytes(LocalSlot::Relationship(*key), overlay)?,
            observer,
        )?;
    }
    Ok(())
}

// Keep merge work proportional to the supplied changes, including for sorted
// BTreeMap input. No accumulated map is rebuilt by a bulk append specialization.
pub(super) fn insert_entries<K: Ord, V>(target: &mut BTreeMap<K, V>, entries: BTreeMap<K, V>) {
    for (key, value) in entries {
        target.insert(key, value);
    }
}

// A singleton BTreeMap owns a whole hidden-capacity node. Reserve sixteen
// inline entries plus header/child pointers per logical slot, deliberately
// overcounting shared nodes. The source and destination data trees coexist
// during movement; the lease table, lease object and spare Vec slots are extra.
pub(super) fn tree_entry_bytes<K, V>() -> StorageResult<u64> {
    (std::mem::size_of::<(K, V)>() as u64)
        .checked_mul(16)
        .and_then(|bytes| bytes.checked_add(256))
        .ok_or_else(snapshot_read::resource_overflow)
}

fn slot_bytes(slot: LocalSlot, overlay: bool) -> StorageResult<u64> {
    let source = match slot {
        LocalSlot::Node(_) => tree_entry_bytes::<i64, DeltaOp>()?,
        LocalSlot::Label(_, _) => tree_entry_bytes::<(i64, i64), DeltaOp>()?,
        LocalSlot::Relationship(_) => tree_entry_bytes::<i64, RelationshipDelta>()?,
        LocalSlot::Property(_, _, _) => tree_entry_bytes::<(OwnerKind, i64, i64), PropertyDelta>()?,
    };
    let target = if overlay {
        overlay_slot_bytes(slot)?
    } else {
        source
    };
    let metadata = snapshot_read::checked_add(tree_entry_bytes::<LocalSlot, ReadLease>()?, 256)?;
    snapshot_read::checked_add(metadata, snapshot_read::checked_add(source, target)?)
}

pub(super) fn overlay_slot_bytes(slot: LocalSlot) -> StorageResult<u64> {
    match slot {
        LocalSlot::Node(_) => tree_entry_bytes::<i64, DeltaOp>(),
        LocalSlot::Label(_, _) => tree_entry_bytes::<(i64, i64), DeltaOp>()?
            .checked_mul(2)
            .ok_or_else(snapshot_read::resource_overflow),
        LocalSlot::Relationship(_) => {
            let adjacency = tree_entry_bytes::<(i64, i64, i64, i64), RelationshipDelta>()?
                .checked_mul(2)
                .ok_or_else(snapshot_read::resource_overflow)?;
            let identity = tree_entry_bytes::<(i64, i64), RelationshipDelta>()?
                .checked_mul(4)
                .ok_or_else(snapshot_read::resource_overflow)?;
            snapshot_read::checked_add(
                tree_entry_bytes::<i64, RelationshipDelta>()?,
                snapshot_read::checked_add(adjacency, identity)?,
            )
        }
        LocalSlot::Property(_, _, _) => snapshot_read::checked_add(
            tree_entry_bytes::<(OwnerKind, i64, i64), PropertyDelta>()?,
            tree_entry_bytes::<(OwnerKind, i64, i64), ReadLease>()?,
        ),
    }
}

impl LocalLayerMemory {
    fn reserve(
        &mut self,
        slot: LocalSlot,
        bytes: u64,
        observer: Option<&dyn SnapshotReadObserver>,
    ) -> StorageResult<()> {
        if let Some(lease) = snapshot_read::reserve(observer, bytes)? {
            self.slots.insert(slot, lease);
        }
        Ok(())
    }
}

fn publish_overlay(overlay: &mut Overlay, delta: PreparedLayer) {
    let mut property_memory = BTreeMap::new();
    let mut temporary = Vec::new();
    for (slot, lease) in delta.memory.slots {
        let exists = match slot {
            LocalSlot::Node(id) => overlay.nodes.contains_key(&id),
            LocalSlot::Label(node, label) => overlay.labels.contains_key(&(node, label)),
            LocalSlot::Relationship(id) => overlay.relationships.contains_key(&id),
            LocalSlot::Property(owner, id, key) => {
                property_memory.insert((owner, id, key), lease);
                continue;
            }
        };
        if exists {
            temporary.push(lease);
        } else {
            overlay.structure_memory.push(lease);
        }
    }
    overlay.apply(delta.layer);
    insert_entries(&mut overlay.property_memory, property_memory);
    drop(temporary);
}
