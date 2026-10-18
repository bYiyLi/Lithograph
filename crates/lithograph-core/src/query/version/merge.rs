use std::collections::{BTreeMap, BTreeSet};

use super::VersionExecution as Connection;
use super::{VersionLayer, VersionOwned, VersionState};

use crate::cypher::{self, Value};
use crate::storage::{self, HashId, MergeSessionRecord, SnapshotState};

use super::{
    ProcedureRow, branch_not_found, commit_value, now_micros, resolve_descriptor, row, string_arg,
    string_value, target_branch,
};
use crate::query::options::ExecutionOptions;
use crate::query::resources::{
    MemoryReservation, allocation_bytes, checked_add, checked_mul, map_entry_bytes,
    value_bytes_controlled,
};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

mod budget;
mod resolutions;
pub(super) use resolutions::load_resolutions;
use resolutions::{encode_resolution, resolution_ids, resolution_items};
mod support;
mod topology;

use support::*;

mod dependencies;
use dependencies::node_dependents_changed;

mod conflict_page;
use conflict_page::{ConflictPage, ConflictPageBuilder};

mod sparse;
pub(crate) use sparse::sparse_candidate_layer;
use sparse::try_sparse_property_merge;

mod state;
mod virtual_base;
use virtual_base::{VirtualValue, from_logical_slots, virtual_base};

pub(crate) use state::{VersionSlots, logical_slots, serde_value, set_logical_slot};

#[derive(Debug, Clone)]
pub(crate) struct MergeConflict {
    pub id: HashId,
    pub slot: String,
    pub base: Option<Value>,
    pub ours: Option<Value>,
    pub theirs: Option<Value>,
    pub resolution: Option<Value>,
}

#[derive(Debug)]
pub(crate) struct MergeComputation {
    pub status: &'static str,
    pub unresolved: usize,
    pub candidate: Option<VersionState>,
}

pub(crate) struct ThreeWayOutcome {
    pub candidate: VersionState,
    pub conflicts: Vec<MergeConflict>,
    pub _conflicts_memory: MemoryReservation,
}

enum FinalizeAction {
    UpToDate,
    FastForward,
    Merge {
        layer: VersionLayer,
        schema_blob: VersionOwned<Vec<u8>>,
    },
}

struct MergeIdentity<'a> {
    base_identity: String,
    ours_commit: HashId,
    ours_identity: String,
    theirs_identity: String,
    resolutions: &'a BTreeMap<HashId, Value>,
}

struct ConflictScanContext<'a> {
    identity: MergeIdentity<'a>,
    base: VersionOwned<BTreeMap<String, VirtualValue>>,
    ours: VersionSlots,
    theirs: VersionSlots,
    ours_state: VersionState,
    slots: VersionOwned<BTreeSet<String>>,
    derived: Vec<MergeConflict>,
    candidate: VersionState,
    _derived_memory: MemoryReservation,
    unresolved: usize,
}

mod procedures;
pub(super) use procedures::{abort, conflicts, finalize, get, list, resolve, start};

pub(crate) fn compute_summary(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<MergeComputation> {
    if let Some(computation) = topology_computation(connection, ours_commit, theirs_commit)? {
        return Ok(computation);
    }
    if let Some(sparse) =
        try_sparse_property_merge(connection, ours_commit, theirs_commit, resolutions)?
    {
        let unresolved = sparse.unresolved_count(connection)?;
        return Ok(MergeComputation {
            status: if unresolved == 0 {
                "ready"
            } else {
                "conflicted"
            },
            unresolved,
            candidate: None,
        });
    }
    let scan = prepare_conflict_scan(connection, ours_commit, theirs_commit, resolutions)?;
    let unresolved = scan.unresolved;
    Ok(MergeComputation {
        status: if unresolved == 0 {
            "ready"
        } else {
            "conflicted"
        },
        unresolved,
        candidate: (unresolved == 0).then_some(scan.candidate),
    })
}

fn prepare_conflict_scan<'a>(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &'a BTreeMap<HashId, Value>,
) -> QueryResult<ConflictScanContext<'a>> {
    let bases = topology::best_common_ancestors(connection, ours_commit, theirs_commit)?;
    if bases.is_empty() {
        return Err(QueryError::new(
            QueryErrorKind::Storage,
            "merge inputs do not share a Root Commit",
        ));
    }
    let base = virtual_base(connection, &bases)?;
    prepare_conflict_scan_from_base(
        connection,
        merge_base_identity(&bases),
        base,
        ours_commit,
        theirs_commit,
        false,
        resolutions,
    )
}

fn prepare_conflict_scan_from_base<'a>(
    connection: &Connection,
    base_identity: String,
    base: VersionOwned<BTreeMap<String, VirtualValue>>,
    ours_commit: HashId,
    theirs_commit: HashId,
    replay_identity: bool,
    resolutions: &'a BTreeMap<HashId, Value>,
) -> QueryResult<ConflictScanContext<'a>> {
    let ours_state = connection.load_state(ours_commit)?;
    let theirs_state = connection.load_state(theirs_commit)?;
    let ours = logical_slots(connection, &ours_state)?;
    let theirs = logical_slots(connection, &theirs_state)?;
    let identity = MergeIdentity {
        base_identity,
        ours_commit,
        ours_identity: if replay_identity {
            budget::logical_identity(connection, &ours)?
        } else {
            commit_identity(ours_commit)
        },
        theirs_identity: commit_identity(theirs_commit),
        resolutions,
    };
    let slots = merge_slots(connection, &base, &ours, &theirs)?;
    let candidate = connection.clone_state(&ours_state)?;
    let mut scan = ConflictScanContext {
        identity,
        base,
        ours,
        theirs,
        ours_state,
        slots,
        derived: Vec::new(),
        candidate,
        unresolved: 0,
        _derived_memory: connection.reserve(0)?,
    };
    populate_candidate(connection, &mut scan)?;
    Ok(scan)
}

fn populate_candidate(
    connection: &Connection,
    scan: &mut ConflictScanContext<'_>,
) -> QueryResult<()> {
    for slot in scan.slots.iter() {
        let conflict = apply_slot_candidate(
            connection,
            &scan.identity,
            &scan.base,
            &scan.ours,
            &scan.theirs,
            &mut scan.candidate,
            slot,
        )?;
        if conflict
            .value
            .is_some_and(|value| value.resolution.is_none())
        {
            scan.unresolved += 1;
        }
    }
    if scan.unresolved == 0 {
        add_schema_validation_conflicts(connection, scan)?;
        scan.unresolved = scan
            .derived
            .iter()
            .filter(|conflict| conflict.resolution.is_none())
            .count();
    }
    scan.derived
        .sort_by(|left, right| left.slot.cmp(&right.slot).then(left.id.cmp(&right.id)));
    Ok(())
}

pub(crate) fn three_way_from_base(
    connection: &Connection,
    base_commit: HashId,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<ThreeWayOutcome> {
    let state = connection.load_state(base_commit)?;
    let base = from_logical_slots(connection, logical_slots(connection, &state)?)?;
    let scan = prepare_conflict_scan_from_base(
        connection,
        merge_base_identity(&[base_commit]),
        base,
        ours_commit,
        theirs_commit,
        true,
        resolutions,
    )?;
    let mut conflicts = Vec::new();
    let mut conflicts_memory = connection.reserve(0)?;
    visit_conflicts(connection, &scan, |conflict, memory| {
        budget::reserve_push(connection, &conflicts, &mut conflicts_memory)?;
        conflicts_memory.merge(memory)?;
        conflicts.push(conflict);
        Ok(false)
    })?;
    Ok(ThreeWayOutcome {
        candidate: scan.candidate,
        conflicts,
        _conflicts_memory: conflicts_memory,
    })
}

fn conflict_page(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &BTreeMap<HashId, Value>,
    offset: usize,
    limit: usize,
) -> QueryResult<(ConflictPage, MemoryReservation)> {
    if topology_computation(connection, ours_commit, theirs_commit)?.is_some() {
        let memory = connection.reserve(allocation_bytes::<MergeConflict>(limit)?)?;
        return Ok((ConflictPageBuilder::new(offset, limit).finish()?, memory));
    }
    if let Some(sparse) =
        try_sparse_property_merge(connection, ours_commit, theirs_commit, resolutions)?
    {
        return sparse.conflict_page(connection, offset, limit);
    }
    let scan = prepare_conflict_scan(connection, ours_commit, theirs_commit, resolutions)?;
    collect_conflict_page(connection, &scan, offset, limit)
}

fn conflicts_for_ids(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &BTreeMap<HashId, Value>,
    requested: &BTreeSet<HashId>,
) -> QueryResult<VersionOwned<BTreeMap<HashId, MergeConflict>>> {
    if requested.is_empty()
        || topology_computation(connection, ours_commit, theirs_commit)?.is_some()
    {
        return Ok(VersionOwned {
            value: BTreeMap::new(),
            memory: connection.reserve(0)?,
        });
    }
    if let Some(sparse) =
        try_sparse_property_merge(connection, ours_commit, theirs_commit, resolutions)?
    {
        return sparse.conflicts_for_ids(connection, requested);
    }
    let scan = prepare_conflict_scan(connection, ours_commit, theirs_commit, resolutions)?;
    let mut found = BTreeMap::new();
    let mut memory = connection.reserve(0)?;
    visit_conflicts(connection, &scan, |conflict, claim| {
        if requested.contains(&conflict.id) {
            memory.grow(map_entry_bytes::<MergeConflict>()?)?;
            memory.merge(claim)?;
            found.insert(conflict.id, conflict);
        }
        Ok(found.len() == requested.len())
    })?;
    Ok(VersionOwned {
        value: found,
        memory,
    })
}

fn collect_conflict_page(
    connection: &Connection,
    scan: &ConflictScanContext<'_>,
    offset: usize,
    limit: usize,
) -> QueryResult<(ConflictPage, MemoryReservation)> {
    let mut memory = connection.reserve(allocation_bytes::<MergeConflict>(limit)?)?;
    let mut builder = ConflictPageBuilder::new(offset, limit);
    visit_conflicts(connection, scan, |conflict, claim| {
        let previous = builder.retained();
        let done = builder.push(conflict);
        if builder.retained() > previous {
            memory.merge(claim)?;
        }
        Ok(done)
    })?;
    Ok((builder.finish()?, memory))
}

fn visit_conflicts(
    connection: &Connection,
    scan: &ConflictScanContext<'_>,
    mut visit: impl FnMut(MergeConflict, MemoryReservation) -> QueryResult<bool>,
) -> QueryResult<()> {
    let mut candidate = connection.clone_state(&scan.ours_state)?;
    let mut derived_index = 0;
    for slot in scan.slots.iter() {
        while derived_index < scan.derived.len() && scan.derived[derived_index].slot < *slot {
            if visit_copied_conflict(connection, &scan.derived[derived_index], &mut visit)? {
                return Ok(());
            }
            derived_index += 1;
        }
        let current =
            collect_slot_conflicts(connection, scan, &mut candidate, slot, &mut derived_index)?;
        for conflict in current.value {
            if visit(conflict.value, conflict.memory)? {
                return Ok(());
            }
        }
    }
    for conflict in &scan.derived[derived_index..] {
        if visit_copied_conflict(connection, conflict, &mut visit)? {
            return Ok(());
        }
    }
    Ok(())
}

type OwnedConflict = VersionOwned<MergeConflict>;

fn visit_copied_conflict(
    connection: &Connection,
    conflict: &MergeConflict,
    visit: &mut impl FnMut(MergeConflict, MemoryReservation) -> QueryResult<bool>,
) -> QueryResult<bool> {
    let conflict = budget::copy_conflict(connection, conflict)?;
    visit(conflict.value, conflict.memory)
}

fn collect_slot_conflicts(
    connection: &Connection,
    scan: &ConflictScanContext<'_>,
    candidate: &mut VersionState,
    slot: &str,
    derived_index: &mut usize,
) -> QueryResult<VersionOwned<Vec<OwnedConflict>>> {
    let mut memory = connection.reserve(allocation_bytes::<OwnedConflict>(2)?)?;
    let mut current = Vec::with_capacity(2);
    let conflict = apply_slot_candidate(
        connection,
        &scan.identity,
        &scan.base,
        &scan.ours,
        &scan.theirs,
        candidate,
        slot,
    )?;
    if let Some(value) = conflict.value {
        current.push(VersionOwned {
            value,
            memory: conflict.memory,
        });
    }
    while *derived_index < scan.derived.len() && scan.derived[*derived_index].slot == slot {
        budget::reserve_push(connection, &current, &mut memory)?;
        current.push(budget::copy_conflict(
            connection,
            &scan.derived[*derived_index],
        )?);
        *derived_index += 1;
    }
    current.sort_unstable_by_key(|conflict| conflict.value.id);
    Ok(VersionOwned {
        value: current,
        memory,
    })
}

fn topology_computation(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<Option<MergeComputation>> {
    if ours_commit == theirs_commit
        || topology::is_ancestor(connection, theirs_commit, ours_commit)?
    {
        return Ok(Some(MergeComputation {
            status: "up_to_date",
            unresolved: 0,
            candidate: Some(connection.load_state(ours_commit)?),
        }));
    }
    if topology::is_ancestor(connection, ours_commit, theirs_commit)? {
        return Ok(Some(MergeComputation {
            status: "fast_forward",
            unresolved: 0,
            candidate: Some(connection.load_state(theirs_commit)?),
        }));
    }
    Ok(None)
}

fn direct_slot_decision(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    slot: String,
    base: VirtualValue,
    ours: Option<Value>,
    theirs: Option<Value>,
) -> QueryResult<VersionOwned<(Option<Value>, Option<MergeConflict>)>> {
    let mut memory = connection.reserve(0)?;
    if super::canonical::same_optional_value_controlled(connection, ours.as_ref(), theirs.as_ref())?
    {
        return Ok(VersionOwned {
            value: (ours, None),
            memory,
        });
    }
    if let VirtualValue::Known(value) = &base {
        if super::canonical::same_optional_value_controlled(
            connection,
            ours.as_ref(),
            value.as_ref(),
        )? {
            return Ok(VersionOwned {
                value: (theirs, None),
                memory,
            });
        }
        if super::canonical::same_optional_value_controlled(
            connection,
            theirs.as_ref(),
            value.as_ref(),
        )? {
            return Ok(VersionOwned {
                value: (ours, None),
                memory,
            });
        }
    }
    let base = match base {
        VirtualValue::Known(value) => value,
        VirtualValue::Unknown => None,
    };
    let conflict = conflict(connection, identity, slot, base, ours, theirs, &mut memory)?;
    let decision = budget::resolve_value(connection, &conflict, identity.resolutions)?;
    memory.merge(decision.memory)?;
    Ok(VersionOwned {
        value: (decision.value, Some(conflict)),
        memory,
    })
}

fn merge_slots(
    connection: &Connection,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
) -> QueryResult<VersionOwned<BTreeSet<String>>> {
    let mut slots = BTreeSet::new();
    let mut memory = connection.reserve(0)?;
    for key in base.keys().chain(ours.keys()).chain(theirs.keys()) {
        connection.checkpoint()?;
        if !slots.contains(key) {
            memory.grow(checked_add(map_entry_bytes::<()>()?, key.len() as u64)?)?;
            slots.insert(crate::query::expression::clone_string_controlled(
                key,
                Some(connection.control()),
            )?);
        }
    }
    Ok(VersionOwned {
        value: slots,
        memory,
    })
}

fn apply_slot_candidate(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    candidate: &mut VersionState,
    slot: &str,
) -> QueryResult<VersionOwned<Option<MergeConflict>>> {
    let VersionOwned {
        value: (decision, direct_conflict),
        mut memory,
    } = prepare_slot_decision(connection, identity, base, ours, theirs, slot)?;
    let dependency = if direct_conflict.is_none() {
        node_delete_modify_conflict(connection, identity, base, ours, theirs, slot, &mut memory)?
    } else {
        None
    };
    let conflict = direct_conflict.or(dependency);
    let decision = match conflict.as_ref() {
        Some(conflict) => {
            let value = budget::resolve_value(connection, conflict, identity.resolutions)?;
            memory.merge(value.memory)?;
            value.value
        }
        None => decision,
    };
    let decision = if node_child_owner(slot).is_some_and(|node| !candidate.nodes.contains(&node)) {
        None
    } else {
        decision
    };
    set_logical_slot(connection, candidate, slot, decision.as_ref())?;
    drop(decision);
    if conflict.is_some() {
        return finish_conflict(connection, conflict, memory);
    }
    drop(memory);
    invalid_relationship_conflict(connection, identity, base, ours, theirs, candidate, slot)
}

fn prepare_slot_decision(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    slot: &str,
) -> QueryResult<VersionOwned<(Option<Value>, Option<MergeConflict>)>> {
    let mut memory = connection.reserve(checked_add(slot.len() as u64, 64)?)?;
    let (ours, ours_memory) =
        budget::copy_optional(connection, effective_slot_value(base, ours, slot))?;
    let (theirs, theirs_memory) =
        budget::copy_optional(connection, effective_slot_value(base, theirs, slot))?;
    memory.merge(ours_memory)?;
    memory.merge(theirs_memory)?;
    let base = copy_base_value(connection, base.get(slot), &mut memory)?;
    let mut decision =
        direct_slot_decision(connection, identity, slot.to_owned(), base, ours, theirs)?;
    decision.memory.merge(memory)?;
    Ok(decision)
}

fn copy_base_value(
    connection: &Connection,
    value: Option<&VirtualValue>,
    memory: &mut MemoryReservation,
) -> QueryResult<VirtualValue> {
    match value {
        Some(VirtualValue::Known(value)) => {
            let (value, claim) = budget::copy_optional(connection, value.as_ref())?;
            memory.merge(claim)?;
            Ok(VirtualValue::Known(value))
        }
        Some(VirtualValue::Unknown) => Ok(VirtualValue::Unknown),
        None => Ok(VirtualValue::Known(None)),
    }
}

fn invalid_relationship_conflict(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    candidate: &mut VersionState,
    slot: &str,
) -> QueryResult<VersionOwned<Option<MergeConflict>>> {
    let mut memory = connection.reserve(checked_add(slot.len() as u64, 64)?)?;
    if !invalid_relationship(candidate, slot) {
        return finish_conflict(connection, None, memory);
    }
    let base = match base.get(slot) {
        Some(VirtualValue::Known(value)) => value.as_ref(),
        _ => None,
    };
    let (base, base_memory) = budget::copy_optional(connection, base)?;
    let (ours, ours_memory) = budget::copy_optional(connection, ours.get(slot))?;
    let (theirs, theirs_memory) = budget::copy_optional(connection, theirs.get(slot))?;
    memory.merge(base_memory)?;
    memory.merge(ours_memory)?;
    memory.merge(theirs_memory)?;
    let conflict = conflict(
        connection,
        identity,
        slot.to_owned(),
        base,
        ours,
        theirs,
        &mut memory,
    )?;
    let choice = budget::resolve_value(connection, &conflict, identity.resolutions)?;
    set_logical_slot(connection, candidate, slot, choice.value.as_ref())?;
    finish_conflict(connection, Some(conflict), memory)
}

fn invalid_relationship(candidate: &VersionState, slot: &str) -> bool {
    relationship_existence_slot(slot)
        .and_then(|id| candidate.relationships.get(&id))
        .is_some_and(|relationship| {
            !candidate.nodes.contains(&relationship.source)
                || !candidate.nodes.contains(&relationship.target)
        })
}

fn finish_conflict(
    connection: &Connection,
    value: Option<MergeConflict>,
    mut memory: MemoryReservation,
) -> QueryResult<VersionOwned<Option<MergeConflict>>> {
    let bytes = match &value {
        Some(value) => budget::conflict_bytes(connection, value)?,
        None => 0,
    };
    memory.resize(bytes)?;
    Ok(VersionOwned { value, memory })
}

fn node_delete_modify_conflict(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    slot: &str,
    memory: &mut MemoryReservation,
) -> QueryResult<Option<MergeConflict>> {
    let Some(node_id) = node_existence_slot(slot) else {
        return Ok(None);
    };
    let Some(VirtualValue::Known(Some(base_value))) = base.get(slot) else {
        return Ok(None);
    };
    let ours_value = ours.get(slot);
    let theirs_value = theirs.get(slot);
    let modified_other_side = match (ours_value.as_ref(), theirs_value.as_ref()) {
        (None, Some(_)) => node_dependents_changed(connection, base, theirs, node_id)?,
        (Some(_), None) => node_dependents_changed(connection, base, ours, node_id)?,
        _ => false,
    };
    if !modified_other_side {
        return Ok(None);
    }
    let (base, base_memory) = budget::copy_optional(connection, Some(base_value))?;
    let (ours, ours_memory) = budget::copy_optional(connection, ours_value)?;
    let (theirs, theirs_memory) = budget::copy_optional(connection, theirs_value)?;
    memory.grow(slot.len() as u64)?;
    memory.merge(base_memory)?;
    memory.merge(ours_memory)?;
    memory.merge(theirs_memory)?;
    conflict(
        connection,
        identity,
        slot.to_owned(),
        base,
        ours,
        theirs,
        memory,
    )
    .map(Some)
}

fn effective_slot_value<'a>(
    base: &'a BTreeMap<String, VirtualValue>,
    side: &'a BTreeMap<String, Value>,
    slot: &str,
) -> Option<&'a Value> {
    let Some(node) = node_child_owner(slot) else {
        return side.get(slot);
    };
    let owner_slot = format!("node/{node}");
    let base_owner_exists = matches!(base.get(&owner_slot), Some(VirtualValue::Known(Some(_))));
    if base_owner_exists && !side.contains_key(&owner_slot) {
        return match base.get(slot) {
            Some(VirtualValue::Known(value)) => value.as_ref(),
            _ => side.get(slot),
        };
    }
    side.get(slot)
}

fn node_child_owner(slot: &str) -> Option<i64> {
    let rest = slot.strip_prefix("node/")?;
    let (node, child) = rest.split_once('/')?;
    if !(child.starts_with("label/") || child.starts_with("property/")) {
        return None;
    }
    node.parse::<i64>().ok().filter(|node| *node > 0)
}

fn node_existence_slot(slot: &str) -> Option<&str> {
    let node = slot.strip_prefix("node/")?;
    (!node.contains('/')).then_some(node)
}

fn relationship_value_references_endpoint(value: &Value, endpoint: &str) -> bool {
    let Value::Map(map) = value else {
        return false;
    };
    ["source", "target"]
        .into_iter()
        .any(|key| matches!(map.get(key), Some(Value::String(value)) if value == endpoint))
}

fn relationship_existence_slot(slot: &str) -> Option<i64> {
    let value = slot.strip_prefix("relationship/")?;
    if value.contains('/') {
        return None;
    }
    value.parse::<i64>().ok().filter(|value| *value > 0)
}

fn add_schema_validation_conflicts(
    connection: &Connection,
    scan: &mut ConflictScanContext<'_>,
) -> QueryResult<()> {
    let ConflictScanContext {
        identity,
        base,
        ours,
        theirs,
        candidate,
        derived: conflicts,
        _derived_memory: memory,
        ..
    } = scan;
    let mut applied = BTreeSet::new();
    let mut resolved = Vec::new();
    loop {
        let (validation, _validation_memory) =
            candidate_validation_conflicts(connection, identity.ours_commit, candidate)?;
        let mut unresolved = Vec::new();
        let mut changed = false;
        for (slot, error) in validation {
            let conflict =
                derived_schema_conflict(connection, identity, &slot, base, ours, theirs, memory)?;
            if conflict.resolution.is_none() {
                budget::reserve_push(connection, &unresolved, memory)?;
                unresolved.push(conflict);
                continue;
            }
            memory.grow(map_entry_bytes::<HashId>()?)?;
            if !applied.insert(conflict.id) {
                return Err(QueryError::invalid_argument(format!(
                    "resolution for derived merge conflict {} does not produce a valid candidate: {}",
                    conflict.id.to_hex(),
                    error.message
                )));
            }
            let replacement = budget::resolve_value(connection, &conflict, identity.resolutions)?;
            set_logical_slot(connection, candidate, &slot, replacement.value.as_ref())?;
            budget::reserve_push(connection, &resolved, memory)?;
            resolved.push(conflict);
            changed = true;
        }
        if changed {
            continue;
        }
        for conflict in resolved.into_iter().chain(unresolved) {
            budget::reserve_push(connection, conflicts, memory)?;
            conflicts.push(conflict);
        }
        break;
    }
    Ok(())
}

fn candidate_validation_conflicts(
    connection: &Connection,
    ours_commit: HashId,
    candidate: &SnapshotState,
) -> QueryResult<(Vec<(String, QueryError)>, MemoryReservation)> {
    let ours = connection.load_state(ours_commit)?;
    let layer = connection.layer_between(&ours, candidate)?;
    let snapshot = connection.with_layer_snapshot(ours_commit, &layer, &candidate.schema)?;
    snapshot.validate_graph_invariants_checked(&super::execution::storage_check(
        connection.control(),
    ))?;
    super::super::schema::validation_conflicts_controlled(
        connection,
        &candidate.schema,
        &snapshot,
        connection.control(),
    )
}

fn derived_schema_conflict(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    slot: &str,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    memory: &mut MemoryReservation,
) -> QueryResult<MergeConflict> {
    let base_value = match base.get(slot) {
        Some(VirtualValue::Known(value)) => value.as_ref(),
        Some(VirtualValue::Unknown) | None => None,
    };
    let (base_value, base_memory) = budget::copy_optional(connection, base_value)?;
    let (ours, ours_memory) = budget::copy_optional(connection, ours.get(slot))?;
    let (theirs, theirs_memory) = budget::copy_optional(connection, theirs.get(slot))?;
    memory.grow(slot.len() as u64)?;
    memory.merge(base_memory)?;
    memory.merge(ours_memory)?;
    memory.merge(theirs_memory)?;
    conflict(
        connection,
        identity,
        slot.to_owned(),
        base_value,
        ours,
        theirs,
        memory,
    )
}

fn conflict(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    slot: String,
    base: Option<Value>,
    ours: Option<Value>,
    theirs: Option<Value>,
    memory: &mut MemoryReservation,
) -> QueryResult<MergeConflict> {
    let id = budget::conflict_hash(
        connection,
        identity,
        &slot,
        base.as_ref(),
        ours.as_ref(),
        theirs.as_ref(),
    )?;
    let (resolution, resolution_memory) =
        budget::copy_optional(connection, identity.resolutions.get(&id))?;
    memory.merge(resolution_memory)?;
    if let Some(resolution) = &resolution {
        validate_resolution_shape(resolution)?;
        validate_resolution_slot_value(&slot, resolution)?;
    }
    Ok(MergeConflict {
        id,
        slot,
        base,
        ours,
        theirs,
        resolution,
    })
}

fn resolution_list(value: Option<&Value>) -> QueryResult<&[Value]> {
    match value {
        Some(Value::List(items)) => Ok(items),
        _ => Err(QueryError::invalid_argument("resolutions must be a List")),
    }
}

fn resolution_map(value: &Value) -> QueryResult<&BTreeMap<String, Value>> {
    match value {
        Value::Map(map) => Ok(map),
        _ => Err(QueryError::invalid_argument(
            "resolution item must be a Map",
        )),
    }
}

fn resolution_conflict_id(map: &BTreeMap<String, Value>) -> QueryResult<HashId> {
    let id = string_map(map, "conflictId")?;
    HashId::from_hex(id)
        .map_err(|_| QueryError::invalid_argument("resolution conflictId is invalid"))
}

fn validate_explicit_value(slot: &str, value: &Value) -> QueryResult<()> {
    if slot.starts_with("node/")
        && !slot.contains("/property/")
        && !slot.contains("/label/")
        && !matches!(value, Value::Boolean(true) | Value::Null)
    {
        return Err(QueryError::invalid_argument(
            "Node existence resolution must be true or null",
        ));
    }
    if slot.contains("/label/") && !matches!(value, Value::Boolean(true) | Value::Null) {
        return Err(QueryError::invalid_argument(
            "Label resolution must be true or null",
        ));
    }
    Ok(())
}

fn validate_resolution_shape(value: &Value) -> QueryResult<()> {
    let Value::Map(map) = value else {
        return Err(QueryError::new(
            QueryErrorKind::Storage,
            "stored merge resolution is invalid",
        ));
    };
    match string_map(map, "choice")? {
        "ours" | "theirs" if !map.contains_key("value") => Ok(()),
        "value" if map.contains_key("value") => Ok(()),
        _ => Err(QueryError::new(
            QueryErrorKind::Storage,
            "stored merge resolution is invalid",
        )),
    }
}

fn validate_resolution_slot_value(slot: &str, resolution: &Value) -> QueryResult<()> {
    let Value::Map(map) = resolution else {
        return Ok(());
    };
    if string_map(map, "choice")? == "value"
        && let Some(value) = map.get("value")
    {
        validate_explicit_value(slot, value)?;
    }
    Ok(())
}

fn session_status_row(
    session: &MergeSessionRecord,
    computation: &MergeComputation,
) -> ProcedureRow {
    let mut output = row([
        ("session", Value::String(session.id.clone())),
        ("targetBranch", Value::String(session.target_branch.clone())),
        ("ours", commit_value(session.ours)),
        ("theirs", commit_value(session.theirs)),
        ("revision", Value::Integer(session.revision)),
    ]);
    procedures::append_computation_status(&mut output, computation);
    output
}
