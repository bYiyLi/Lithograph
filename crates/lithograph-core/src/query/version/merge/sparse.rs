use std::collections::{BTreeMap, BTreeSet};

use crate::query::resources::{MemoryReservation, checked_add, map_entry_bytes};
use crate::query::version::{
    VersionExecution as Connection, VersionLayer, VersionOwned, VersionSchema,
};

use crate::cypher::Value;
use crate::storage::{self, HashId, LayerBuilder, OwnerKind, PropertyKeyId, SchemaState};

use super::budget::copy_optional;
use super::*;

type PropertySlot = (OwnerKind, i64, PropertyKeyId);

struct SparsePropertySlot {
    owner: OwnerKind,
    owner_id: i64,
    key_id: PropertyKeyId,
    base: Option<Value>,
    ours: Option<Value>,
    theirs: Option<Value>,
}

pub(super) struct SparsePropertyMerge<'a> {
    identity: MergeIdentity<'a>,
    slots: BTreeMap<String, SparsePropertySlot>,
    pub(super) ours_schema: VersionSchema,
    _memory: MemoryReservation,
}

struct SparseMergeInputs {
    bases: VersionOwned<Vec<HashId>>,
    base_commit: HashId,
    ours_layer: VersionLayer,
    theirs_layer: VersionLayer,
    ours_schema: VersionSchema,
}

struct SparseMap<K, V> {
    values: BTreeMap<K, V>,
    memory: MemoryReservation,
}

impl<K: Ord, V> SparseMap<K, V> {
    fn new(connection: &Connection) -> QueryResult<Self> {
        Ok(Self {
            values: BTreeMap::new(),
            memory: connection.reserve(0)?,
        })
    }

    fn reserve_entry(&mut self, key_bytes: u64) -> QueryResult<()> {
        self.memory
            .grow(checked_add(map_entry_bytes::<V>()?, key_bytes)?)
    }
}

pub(crate) fn sparse_candidate_layer(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<Option<(VersionLayer, VersionSchema, usize)>> {
    let Some(sparse) =
        try_sparse_property_merge(connection, ours_commit, theirs_commit, resolutions)?
    else {
        return Ok(None);
    };
    let unresolved = sparse.unresolved_count(connection)?;
    let layer = if unresolved == 0 {
        sparse.candidate_layer(connection)?
    } else {
        connection.empty_layer()?
    };
    Ok(Some((layer, sparse.ours_schema, unresolved)))
}

pub(super) fn try_sparse_property_merge<'a>(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &'a BTreeMap<HashId, Value>,
) -> QueryResult<Option<SparsePropertyMerge<'a>>> {
    let Some(inputs) = sparse_merge_inputs(connection, ours_commit, theirs_commit)? else {
        return Ok(None);
    };
    let ours_changes = sparse_property_changes(connection, &inputs.ours_layer)?;
    let theirs_changes = sparse_property_changes(connection, &inputs.theirs_layer)?;
    let (touched, _touched_memory) =
        touched_property_slots(connection, &ours_changes.values, &theirs_changes.values)?;
    let Some(key_names) = sparse_property_key_names(connection, &inputs.ours_schema, &touched)?
    else {
        return Ok(None);
    };
    let mut slots = sparse_property_slots(
        connection,
        inputs.base_commit,
        &touched,
        &key_names.values,
        &ours_changes.values,
        &theirs_changes.values,
    )?;
    slots.memory.grow(512)?;
    Ok(Some(SparsePropertyMerge {
        identity: MergeIdentity {
            base_identity: merge_base_identity(&inputs.bases),
            ours_commit,
            ours_identity: commit_identity(ours_commit),
            theirs_identity: commit_identity(theirs_commit),
            resolutions,
        },
        slots: slots.values,
        _memory: slots.memory,
        ours_schema: inputs.ours_schema,
    }))
}

fn sparse_merge_inputs(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<Option<SparseMergeInputs>> {
    let Some((bases, base_commit)) = sparse_merge_base(connection, ours_commit, theirs_commit)?
    else {
        return Ok(None);
    };
    if !sparse_schema_hashes_match(connection, base_commit, ours_commit, theirs_commit)? {
        return Ok(None);
    }
    let Some((ours_layer, theirs_layer, ours_schema)) =
        sparse_property_inputs(connection, base_commit, ours_commit, theirs_commit)?
    else {
        return Ok(None);
    };
    Ok(Some(SparseMergeInputs {
        bases,
        base_commit,
        ours_layer,
        theirs_layer,
        ours_schema,
    }))
}

fn sparse_property_inputs(
    connection: &Connection,
    base_commit: HashId,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<Option<(VersionLayer, VersionLayer, VersionSchema)>> {
    let ours_layer = connection.layer_between_commits(base_commit, ours_commit)?;
    let theirs_layer = connection.layer_between_commits(base_commit, theirs_commit)?;
    if !ours_layer.is_property_only() || !theirs_layer.is_property_only() {
        return Ok(None);
    }
    let ours_schema = connection.load_schema(ours_commit)?;
    if !ours_schema.graph_nodes.is_empty() || !ours_schema.graph_relationships.is_empty() {
        return Ok(None);
    }
    Ok(Some((ours_layer, theirs_layer, ours_schema)))
}

fn sparse_merge_base(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<Option<(VersionOwned<Vec<HashId>>, HashId)>> {
    let bases = super::topology::best_common_ancestors(connection, ours_commit, theirs_commit)?;
    if bases.len() != 1 {
        return Ok(None);
    }
    let base_commit = bases[0];
    if !super::topology::is_first_parent_descendant(connection, base_commit, ours_commit)?
        || !super::topology::is_first_parent_descendant(connection, base_commit, theirs_commit)?
    {
        return Ok(None);
    }
    Ok(Some((bases, base_commit)))
}

fn sparse_schema_hashes_match(
    connection: &Connection,
    base_commit: HashId,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<bool> {
    let base_record = storage::load_commit(connection, base_commit)?;
    let ours_record = storage::load_commit(connection, ours_commit)?;
    let theirs_record = storage::load_commit(connection, theirs_commit)?;
    Ok(base_record.schema_hash == ours_record.schema_hash
        && base_record.schema_hash == theirs_record.schema_hash)
}

fn touched_property_slots(
    connection: &Connection,
    ours_changes: &BTreeMap<PropertySlot, Option<Value>>,
    theirs_changes: &BTreeMap<PropertySlot, Option<Value>>,
) -> QueryResult<(BTreeSet<PropertySlot>, MemoryReservation)> {
    let mut touched = BTreeSet::new();
    let mut memory = connection.reserve(0)?;
    for slot in ours_changes.keys().chain(theirs_changes.keys()) {
        connection.checkpoint()?;
        if !touched.contains(slot) {
            memory.grow(512)?;
            touched.insert(*slot);
        }
    }
    Ok((touched, memory))
}

fn sparse_property_key_names(
    connection: &Connection,
    schema: &SchemaState,
    touched: &BTreeSet<PropertySlot>,
) -> QueryResult<Option<SparseMap<PropertyKeyId, String>>> {
    let mut key_names = SparseMap::new(connection)?;
    for (_, _, key_id) in touched {
        connection.checkpoint()?;
        if key_names.values.contains_key(key_id) {
            continue;
        }
        let found = storage::with_property_key_name(connection, *key_id, |key_name| {
            let key_name = key_name.ok_or_else(|| {
                QueryError::new(
                    QueryErrorKind::Storage,
                    format!("PropertyKeyId {key_id} is missing"),
                )
            })?;
            for constraint in schema.constraints.values() {
                for property in &constraint.properties {
                    connection.checkpoint()?;
                    if property == key_name {
                        return Ok::<_, QueryError>(false);
                    }
                }
            }
            key_names.reserve_entry(key_name.len() as u64)?;
            let name = crate::query::expression::clone_string_controlled(
                key_name,
                Some(connection.control()),
            )?;
            key_names.values.insert(*key_id, name);
            Ok(true)
        })?;
        if !found {
            return Ok(None);
        }
    }
    Ok(Some(key_names))
}

fn sparse_property_slots(
    connection: &Connection,
    base_commit: HashId,
    touched: &BTreeSet<PropertySlot>,
    key_names: &BTreeMap<PropertyKeyId, String>,
    ours_changes: &BTreeMap<PropertySlot, Option<Value>>,
    theirs_changes: &BTreeMap<PropertySlot, Option<Value>>,
) -> QueryResult<SparseMap<String, SparsePropertySlot>> {
    let base_snapshot = crate::query::graph::resolve_snapshot_with_control(
        connection,
        base_commit,
        connection.control(),
    )?;
    let mut slots = SparseMap::new(connection)?;
    for property_slot @ (owner, owner_id, key_id) in touched.iter().copied() {
        connection.checkpoint()?;
        let key_name = &key_names[&key_id];
        slots.reserve_entry(checked_add(key_name.len() as u64, 96)?)?;
        let (base, base_memory) =
            base_property(connection, &base_snapshot, owner, owner_id, key_id)?;
        let (ours, ours_memory) = copy_optional(
            connection,
            ours_changes.get(&property_slot).unwrap_or(&base).as_ref(),
        )?;
        let (theirs, theirs_memory) = copy_optional(
            connection,
            theirs_changes.get(&property_slot).unwrap_or(&base).as_ref(),
        )?;
        slots.memory.merge(base_memory)?;
        slots.memory.merge(ours_memory)?;
        slots.memory.merge(theirs_memory)?;
        let slot = match owner {
            OwnerKind::Node => format!("node/{owner_id}/property/{key_name}"),
            OwnerKind::Relationship => {
                format!("relationship/{owner_id}/property/{key_name}")
            }
        };
        slots.values.insert(
            slot,
            SparsePropertySlot {
                owner,
                owner_id,
                key_id,
                base,
                ours,
                theirs,
            },
        );
    }
    Ok(slots)
}

fn base_property(
    connection: &Connection,
    snapshot: &storage::Snapshot<'_>,
    owner: OwnerKind,
    owner_id: i64,
    key_id: PropertyKeyId,
) -> QueryResult<(Option<Value>, MemoryReservation)> {
    let check = crate::query::version::execution::storage_check(connection.control());
    snapshot.with_property_checked(owner, owner_id, key_id, &check, |value| match value {
        Some(value) => connection
            .property_value(value)
            .map(|(value, memory)| (Some(value), memory)),
        None => Ok::<_, QueryError>((None, connection.reserve(0)?)),
    })
}

fn sparse_property_changes(
    connection: &Connection,
    layer: &LayerBuilder,
) -> QueryResult<SparseMap<PropertySlot, Option<Value>>> {
    let mut changes = SparseMap::new(connection)?;
    for (owner, owner_id, key_id, property) in layer.property_changes() {
        connection.checkpoint()?;
        changes.reserve_entry(0)?;
        let value = if let Some(property) = property {
            let (value, memory) = connection.property_value(property)?;
            changes.memory.merge(memory)?;
            Some(value)
        } else {
            None
        };
        changes.values.insert((owner, owner_id, key_id), value);
    }
    Ok(changes)
}

fn apply_candidate_property(
    connection: &Connection,
    layer: &mut VersionLayer,
    slot: &SparsePropertySlot,
    decision: Option<Value>,
    mut memory: MemoryReservation,
) -> QueryResult<()> {
    layer
        .memory
        .grow(map_entry_bytes::<storage::PropertyValue>()?)?;
    if let Some(value) = decision {
        let property =
            crate::query::mutation::property_from_value_controlled(value, connection.control())?
                .ok_or_else(|| {
                    QueryError::invalid_argument("property resolution cannot be null")
                })?;
        let check = crate::query::version::execution::storage_check(connection.control());
        memory.resize(storage::property_value_memory_bytes(&property, &check)?)?;
        layer.memory.merge(memory)?;
        layer.set_property(slot.owner, slot.owner_id, slot.key_id, property)?;
    } else {
        layer.remove_property(slot.owner, slot.owner_id, slot.key_id)?;
    }
    Ok(())
}

impl SparsePropertyMerge<'_> {
    fn slot_decision(
        &self,
        connection: &Connection,
        slot: &str,
        value: &SparsePropertySlot,
    ) -> QueryResult<VersionOwned<(Option<Value>, Option<MergeConflict>)>> {
        connection.checkpoint()?;
        let mut memory = connection.reserve(slot.len() as u64)?;
        let (base, base_memory) = copy_optional(connection, value.base.as_ref())?;
        let (ours, ours_memory) = copy_optional(connection, value.ours.as_ref())?;
        let (theirs, theirs_memory) = copy_optional(connection, value.theirs.as_ref())?;
        memory.merge(base_memory)?;
        memory.merge(ours_memory)?;
        memory.merge(theirs_memory)?;
        let mut decision = direct_slot_decision(
            connection,
            &self.identity,
            slot.to_owned(),
            VirtualValue::Known(base),
            ours,
            theirs,
        )?;
        decision.memory.merge(memory)?;
        Ok(decision)
    }

    pub(super) fn unresolved_count(&self, connection: &Connection) -> QueryResult<usize> {
        let mut unresolved = 0_usize;
        for (slot, value) in &self.slots {
            if self
                .slot_decision(connection, slot, value)?
                .value
                .1
                .is_some_and(|conflict| conflict.resolution.is_none())
            {
                unresolved += 1;
            }
        }
        Ok(unresolved)
    }

    fn candidate_layer(&self, connection: &Connection) -> QueryResult<VersionLayer> {
        let mut layer = connection.empty_layer()?;
        for (slot, property_slot) in &self.slots {
            let VersionOwned {
                value: (decision, conflict),
                memory,
            } = self.slot_decision(connection, slot, property_slot)?;
            if super::super::canonical::same_optional_value_controlled(
                connection,
                decision.as_ref(),
                property_slot.ours.as_ref(),
            )? {
                continue;
            }
            drop(conflict);
            apply_candidate_property(connection, &mut layer, property_slot, decision, memory)?;
        }
        Ok(layer)
    }

    pub(super) fn conflict_page(
        &self,
        connection: &Connection,
        offset: usize,
        limit: usize,
    ) -> QueryResult<(ConflictPage, MemoryReservation)> {
        let mut memory = connection.reserve(crate::query::resources::allocation_bytes::<
            MergeConflict,
        >(limit)?)?;
        let mut builder = ConflictPageBuilder::new(offset, limit);
        let mut seen = 0;
        let mut kept = 0;
        for (slot, value) in &self.slots {
            let VersionOwned {
                value: (_, conflict),
                memory: conflict_memory,
            } = self.slot_decision(connection, slot, value)?;
            if let Some(conflict) = conflict {
                if seen >= offset && kept < limit {
                    memory.merge(conflict_memory)?;
                    kept += 1;
                }
                seen += 1;
                if builder.push(conflict) {
                    break;
                }
            }
        }
        Ok((builder.finish()?, memory))
    }

    pub(super) fn conflicts_for_ids(
        &self,
        connection: &Connection,
        requested: &BTreeSet<HashId>,
    ) -> QueryResult<VersionOwned<BTreeMap<HashId, MergeConflict>>> {
        let mut found = VersionOwned {
            value: BTreeMap::new(),
            memory: connection.reserve(0)?,
        };
        for (slot, value) in &self.slots {
            let VersionOwned {
                value: (_, conflict),
                memory,
            } = self.slot_decision(connection, slot, value)?;
            if let Some(conflict) = conflict
                && requested.contains(&conflict.id)
            {
                found
                    .memory
                    .grow(checked_add(map_entry_bytes::<MergeConflict>()?, 128)?)?;
                found.memory.merge(memory)?;
                found.value.insert(conflict.id, conflict);
                if found.len() == requested.len() {
                    break;
                }
            }
        }
        Ok(found)
    }
}
