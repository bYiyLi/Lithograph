use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{self, Value};
use crate::storage::{self, CommitMetadata, HashId, MergeSessionRecord, SnapshotState};

use super::canonical::same_optional_value;
use super::{
    ProcedureRow, branch_not_found, commit_value, now_micros, resolve_descriptor, row, string_arg,
    string_value, target_branch,
};
use crate::query::options::ExecutionOptions;
use crate::query::{QueryError, QueryErrorKind, QueryResult, schema as qs};

mod support;

use support::*;

mod conflict_page;
use conflict_page::{ConflictPage, ConflictPageBuilder};

mod sparse;
pub(crate) use sparse::sparse_candidate_layer;
use sparse::try_sparse_property_merge;

mod state;

pub(crate) use state::{logical_slots, set_logical_slot};

#[derive(Debug, Clone)]
enum VirtualValue {
    Known(Option<Value>),
    Unknown,
}

impl PartialEq for VirtualValue {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Known(left), Self::Known(right)) => {
                same_optional_value(left.as_ref(), right.as_ref())
            }
            (Self::Unknown, Self::Unknown) => true,
            _ => false,
        }
    }
}

#[derive(Debug, Clone)]
pub(crate) struct MergeConflict {
    pub id: HashId,
    pub slot: String,
    pub base: Option<Value>,
    pub ours: Option<Value>,
    pub theirs: Option<Value>,
    pub resolution: Option<Value>,
}

#[derive(Debug, Clone)]
pub(crate) struct MergeComputation {
    pub status: &'static str,
    pub unresolved: usize,
    pub candidate: Option<SnapshotState>,
}

pub(crate) struct ThreeWayOutcome {
    pub candidate: SnapshotState,
    pub conflicts: Vec<MergeConflict>,
}

enum FinalizeAction {
    UpToDate,
    FastForward,
    Merge {
        layer: storage::LayerBuilder,
        schema_blob: Vec<u8>,
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
    base: BTreeMap<String, VirtualValue>,
    ours: BTreeMap<String, Value>,
    theirs: BTreeMap<String, Value>,
    ours_state: SnapshotState,
    slots: BTreeSet<String>,
    derived: Vec<MergeConflict>,
    candidate: SnapshotState,
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
        let unresolved = sparse.unresolved_count()?;
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
    let bases = storage::best_common_ancestors(connection, ours_commit, theirs_commit)?;
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
    base: BTreeMap<String, VirtualValue>,
    ours_commit: HashId,
    theirs_commit: HashId,
    replay_identity: bool,
    resolutions: &'a BTreeMap<HashId, Value>,
) -> QueryResult<ConflictScanContext<'a>> {
    let ours_state = storage::load_snapshot_state(connection, ours_commit)?;
    let theirs_state = storage::load_snapshot_state(connection, theirs_commit)?;
    let ours = logical_slots(connection, &ours_state)?;
    let theirs = logical_slots(connection, &theirs_state)?;
    let identity = MergeIdentity {
        base_identity,
        ours_commit,
        ours_identity: if replay_identity {
            logical_state_identity(&ours)?
        } else {
            commit_identity(ours_commit)
        },
        theirs_identity: commit_identity(theirs_commit),
        resolutions,
    };
    let slots = merge_slots(&base, &ours, &theirs);
    let mut candidate = ours_state.clone();
    let mut unresolved = 0_usize;
    for slot in &slots {
        if let Some(conflict) = apply_slot_candidate(
            connection,
            &identity,
            &base,
            &ours,
            &theirs,
            &mut candidate,
            slot,
        )? && conflict.resolution.is_none()
        {
            unresolved += 1;
        }
    }
    let mut derived = Vec::new();
    if unresolved == 0 {
        add_schema_validation_conflicts(
            connection,
            &identity,
            &base,
            &ours,
            &theirs,
            &mut candidate,
            &mut derived,
        )?;
        unresolved = derived
            .iter()
            .filter(|conflict| conflict.resolution.is_none())
            .count();
    }
    derived.sort_by(|left, right| left.slot.cmp(&right.slot).then(left.id.cmp(&right.id)));
    Ok(ConflictScanContext {
        identity,
        base,
        ours,
        theirs,
        ours_state,
        slots,
        derived,
        candidate,
        unresolved,
    })
}

pub(crate) fn three_way_from_base(
    connection: &Connection,
    base_commit: HashId,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<ThreeWayOutcome> {
    let base = logical_slots(
        connection,
        &storage::load_snapshot_state(connection, base_commit)?,
    )?
    .into_iter()
    .map(|(slot, value)| (slot, VirtualValue::Known(Some(value))))
    .collect();
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
    visit_conflicts(connection, &scan, |conflict| {
        conflicts.push(conflict);
        false
    })?;
    Ok(ThreeWayOutcome {
        candidate: scan.candidate,
        conflicts,
    })
}

fn conflict_page(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
    resolutions: &BTreeMap<HashId, Value>,
    offset: usize,
    limit: usize,
) -> QueryResult<ConflictPage> {
    if topology_computation(connection, ours_commit, theirs_commit)?.is_some() {
        return ConflictPageBuilder::new(offset, limit).finish();
    }
    if let Some(sparse) =
        try_sparse_property_merge(connection, ours_commit, theirs_commit, resolutions)?
    {
        return sparse.conflict_page(offset, limit);
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
) -> QueryResult<BTreeMap<HashId, MergeConflict>> {
    if requested.is_empty()
        || topology_computation(connection, ours_commit, theirs_commit)?.is_some()
    {
        return Ok(BTreeMap::new());
    }
    if let Some(sparse) =
        try_sparse_property_merge(connection, ours_commit, theirs_commit, resolutions)?
    {
        return sparse.conflicts_for_ids(requested);
    }
    let scan = prepare_conflict_scan(connection, ours_commit, theirs_commit, resolutions)?;
    let mut found = BTreeMap::new();
    visit_conflicts(connection, &scan, |conflict| {
        if requested.contains(&conflict.id) {
            found.insert(conflict.id, conflict);
        }
        found.len() == requested.len()
    })?;
    Ok(found)
}

fn collect_conflict_page(
    connection: &Connection,
    scan: &ConflictScanContext<'_>,
    offset: usize,
    limit: usize,
) -> QueryResult<ConflictPage> {
    let mut builder = ConflictPageBuilder::new(offset, limit);
    visit_conflicts(connection, scan, |conflict| builder.push(conflict))?;
    builder.finish()
}

fn visit_conflicts(
    connection: &Connection,
    scan: &ConflictScanContext<'_>,
    mut visit: impl FnMut(MergeConflict) -> bool,
) -> QueryResult<()> {
    let mut candidate = scan.ours_state.clone();
    let mut derived_index = 0_usize;
    for slot in &scan.slots {
        while derived_index < scan.derived.len() && scan.derived[derived_index].slot < *slot {
            if visit(scan.derived[derived_index].clone()) {
                return Ok(());
            }
            derived_index += 1;
        }
        let mut current = Vec::with_capacity(2);
        if let Some(conflict) = apply_slot_candidate(
            connection,
            &scan.identity,
            &scan.base,
            &scan.ours,
            &scan.theirs,
            &mut candidate,
            slot,
        )? {
            current.push(conflict);
        }
        while derived_index < scan.derived.len() && scan.derived[derived_index].slot == *slot {
            current.push(scan.derived[derived_index].clone());
            derived_index += 1;
        }
        current.sort_by_key(|conflict| conflict.id);
        for conflict in current {
            if visit(conflict) {
                return Ok(());
            }
        }
    }
    for conflict in &scan.derived[derived_index..] {
        if visit(conflict.clone()) {
            return Ok(());
        }
    }
    Ok(())
}

fn topology_computation(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<Option<MergeComputation>> {
    if ours_commit == theirs_commit || storage::is_ancestor(connection, theirs_commit, ours_commit)?
    {
        return Ok(Some(MergeComputation {
            status: "up_to_date",
            unresolved: 0,
            candidate: Some(storage::load_snapshot_state(connection, ours_commit)?),
        }));
    }
    if storage::is_ancestor(connection, ours_commit, theirs_commit)? {
        return Ok(Some(MergeComputation {
            status: "fast_forward",
            unresolved: 0,
            candidate: Some(storage::load_snapshot_state(connection, theirs_commit)?),
        }));
    }
    Ok(None)
}

fn direct_slot_decision(
    identity: &MergeIdentity<'_>,
    slot: String,
    base: VirtualValue,
    ours: Option<Value>,
    theirs: Option<Value>,
) -> QueryResult<(Option<Value>, Option<MergeConflict>)> {
    if same_optional_value(ours.as_ref(), theirs.as_ref()) {
        return Ok((ours, None));
    }
    match base {
        VirtualValue::Known(base) if same_optional_value(ours.as_ref(), base.as_ref()) => {
            Ok((theirs, None))
        }
        VirtualValue::Known(base) if same_optional_value(theirs.as_ref(), base.as_ref()) => {
            Ok((ours, None))
        }
        VirtualValue::Known(base) => {
            let conflict = conflict(identity, slot, base, ours, theirs)?;
            Ok((
                resolved_value(&conflict, identity.resolutions)?,
                Some(conflict),
            ))
        }
        VirtualValue::Unknown => {
            let conflict = conflict(identity, slot, None, ours, theirs)?;
            Ok((
                resolved_value(&conflict, identity.resolutions)?,
                Some(conflict),
            ))
        }
    }
}

fn merge_slots(
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
) -> BTreeSet<String> {
    let mut slots = BTreeSet::new();
    slots.extend(base.keys().cloned());
    slots.extend(ours.keys().cloned());
    slots.extend(theirs.keys().cloned());
    slots
}

fn apply_slot_candidate(
    connection: &Connection,
    identity: &MergeIdentity<'_>,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    candidate: &mut SnapshotState,
    slot: &str,
) -> QueryResult<Option<MergeConflict>> {
    let ours_value = effective_slot_value(base, ours, slot);
    let theirs_value = effective_slot_value(base, theirs, slot);
    let base_value = base.get(slot).cloned().unwrap_or(VirtualValue::Known(None));
    let (decision, direct_conflict) = direct_slot_decision(
        identity,
        slot.to_owned(),
        base_value,
        ours_value,
        theirs_value,
    )?;
    let dependency_conflict = if direct_conflict.is_none() {
        node_delete_modify_conflict(identity, base, ours, theirs, slot)?
    } else {
        None
    };
    let slot_conflict = direct_conflict.or(dependency_conflict);
    let decision = match slot_conflict.as_ref() {
        Some(conflict) => resolved_value(conflict, identity.resolutions)?,
        None => decision,
    };
    let decision = if node_child_owner(slot).is_some_and(|node| !candidate.nodes.contains(&node)) {
        None
    } else {
        decision
    };
    set_logical_slot(connection, candidate, slot, decision.as_ref())?;
    if slot_conflict.is_some() {
        return Ok(slot_conflict);
    }
    let Some(relationship_id) = relationship_existence_slot(slot) else {
        return Ok(None);
    };
    let invalid = candidate
        .relationships
        .get(&relationship_id)
        .is_some_and(|relationship| {
            !candidate.nodes.contains(&relationship.source)
                || !candidate.nodes.contains(&relationship.target)
        });
    if !invalid {
        return Ok(None);
    }
    let base_value = match base.get(slot) {
        Some(VirtualValue::Known(value)) => value.clone(),
        _ => None,
    };
    let conflict = conflict(
        identity,
        slot.to_owned(),
        base_value,
        ours.get(slot).cloned(),
        theirs.get(slot).cloned(),
    )?;
    let choice = resolved_value(&conflict, identity.resolutions)?;
    set_logical_slot(connection, candidate, slot, choice.as_ref())?;
    Ok(Some(conflict))
}

fn node_delete_modify_conflict(
    identity: &MergeIdentity<'_>,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    slot: &str,
) -> QueryResult<Option<MergeConflict>> {
    let Some(node_id) = node_existence_slot(slot) else {
        return Ok(None);
    };
    let Some(VirtualValue::Known(Some(base_value))) = base.get(slot) else {
        return Ok(None);
    };
    let ours_value = ours.get(slot).cloned();
    let theirs_value = theirs.get(slot).cloned();
    let modified_other_side = match (ours_value.as_ref(), theirs_value.as_ref()) {
        (None, Some(_)) => node_dependents_changed(base, theirs, node_id),
        (Some(_), None) => node_dependents_changed(base, ours, node_id),
        _ => false,
    };
    if !modified_other_side {
        return Ok(None);
    }
    conflict(
        identity,
        slot.to_owned(),
        Some(base_value.clone()),
        ours_value,
        theirs_value,
    )
    .map(Some)
}

fn node_dependents_changed(
    base: &BTreeMap<String, VirtualValue>,
    side: &BTreeMap<String, Value>,
    node_id: &str,
) -> bool {
    node_children_changed(base, side, node_id)
        || relationship_dependencies_changed(base, side, node_id)
}

fn effective_slot_value(
    base: &BTreeMap<String, VirtualValue>,
    side: &BTreeMap<String, Value>,
    slot: &str,
) -> Option<Value> {
    let Some(node) = node_child_owner(slot) else {
        return side.get(slot).cloned();
    };
    let owner_slot = format!("node/{node}");
    let base_owner_exists = matches!(base.get(&owner_slot), Some(VirtualValue::Known(Some(_))));
    if base_owner_exists && !side.contains_key(&owner_slot) {
        return match base.get(slot) {
            Some(VirtualValue::Known(value)) => value.clone(),
            _ => side.get(slot).cloned(),
        };
    }
    side.get(slot).cloned()
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

fn node_children_changed(
    base: &BTreeMap<String, VirtualValue>,
    side: &BTreeMap<String, Value>,
    node_id: &str,
) -> bool {
    let prefix = format!("node/{node_id}/");
    let mut slots = BTreeSet::new();
    slots.extend(
        base.keys()
            .filter(|slot| slot.starts_with(&prefix))
            .cloned(),
    );
    slots.extend(
        side.keys()
            .filter(|slot| slot.starts_with(&prefix))
            .cloned(),
    );
    slots.into_iter().any(|slot| {
        let side_value = side.get(&slot);
        match base.get(&slot) {
            Some(VirtualValue::Known(base_value)) => {
                !same_optional_value(base_value.as_ref(), side_value)
            }
            Some(VirtualValue::Unknown) => true,
            None => side_value.is_some(),
        }
    })
}

fn relationship_dependencies_changed(
    base: &BTreeMap<String, VirtualValue>,
    side: &BTreeMap<String, Value>,
    node_id: &str,
) -> bool {
    let endpoint = format!("n:{node_id}");
    side.iter().any(|(slot, value)| {
        if relationship_existence_slot(slot).is_none()
            || !relationship_value_references_endpoint(value, &endpoint)
        {
            return false;
        }
        if match base.get(slot) {
            Some(VirtualValue::Known(base_value)) => {
                !same_optional_value(base_value.as_ref(), Some(value))
            }
            Some(VirtualValue::Unknown) => true,
            None => true,
        } {
            return true;
        }
        let prefix = format!("{slot}/property/");
        let mut properties = BTreeSet::new();
        properties.extend(
            base.keys()
                .filter(|candidate| candidate.starts_with(&prefix))
                .cloned(),
        );
        properties.extend(
            side.keys()
                .filter(|candidate| candidate.starts_with(&prefix))
                .cloned(),
        );
        properties.into_iter().any(|property| {
            let side_value = side.get(&property);
            match base.get(&property) {
                Some(VirtualValue::Known(base_value)) => {
                    !same_optional_value(base_value.as_ref(), side_value)
                }
                Some(VirtualValue::Unknown) => true,
                None => side_value.is_some(),
            }
        })
    })
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
    identity: &MergeIdentity<'_>,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
    candidate: &mut SnapshotState,
    conflicts: &mut Vec<MergeConflict>,
) -> QueryResult<()> {
    let mut applied = BTreeSet::new();
    let mut resolved = Vec::new();
    loop {
        let validation =
            candidate_validation_conflicts(connection, identity.ours_commit, candidate)?;
        let mut unresolved = Vec::new();
        let mut changed = false;
        for (slot, error) in validation {
            let conflict = derived_schema_conflict(identity, &slot, base, ours, theirs)?;
            if conflict.resolution.is_none() {
                unresolved.push(conflict);
                continue;
            }
            if !applied.insert(conflict.id) {
                return Err(QueryError::invalid_argument(format!(
                    "resolution for derived merge conflict {} does not produce a valid candidate: {}",
                    conflict.id.to_hex(),
                    error.message
                )));
            }
            resolved.push(conflict.clone());
            let replacement = resolved_value(&conflict, identity.resolutions)?;
            set_logical_slot(connection, candidate, &slot, replacement.as_ref())?;
            changed = true;
        }
        if changed {
            continue;
        }
        conflicts.extend(resolved);
        conflicts.extend(unresolved);
        break;
    }
    Ok(())
}

fn candidate_validation_conflicts(
    connection: &Connection,
    ours_commit: HashId,
    candidate: &SnapshotState,
) -> QueryResult<Vec<(String, QueryError)>> {
    let ours = storage::load_snapshot_state(connection, ours_commit)?;
    let layer = storage::layer_between(&ours, candidate)?;
    let snapshot = storage::Snapshot::resolve_with_layer_and_schema(
        connection,
        ours_commit,
        &layer,
        candidate.schema.clone(),
    )?;
    snapshot.validate_graph_invariants()?;
    super::super::schema::validation_conflicts(connection, &candidate.schema, &snapshot)
}

fn derived_schema_conflict(
    identity: &MergeIdentity<'_>,
    slot: &str,
    base: &BTreeMap<String, VirtualValue>,
    ours: &BTreeMap<String, Value>,
    theirs: &BTreeMap<String, Value>,
) -> QueryResult<MergeConflict> {
    let base_value = match base.get(slot) {
        Some(VirtualValue::Known(value)) => value.clone(),
        Some(VirtualValue::Unknown) | None => None,
    };
    conflict(
        identity,
        slot.to_owned(),
        base_value,
        ours.get(slot).cloned(),
        theirs.get(slot).cloned(),
    )
}

fn virtual_base(
    connection: &Connection,
    bases: &[HashId],
) -> QueryResult<BTreeMap<String, VirtualValue>> {
    if bases.len() == 1 {
        return logical_slots(
            connection,
            &storage::load_snapshot_state(connection, bases[0])?,
        )
        .map(|slots| {
            slots
                .into_iter()
                .map(|(slot, value)| (slot, VirtualValue::Known(Some(value))))
                .collect()
        });
    }
    let mut current = virtual_merge_commits(connection, bases[0], bases[1])?;
    for commit in &bases[2..] {
        let right = logical_slots(
            connection,
            &storage::load_snapshot_state(connection, *commit)?,
        )?;
        let mut slots = BTreeSet::new();
        slots.extend(current.keys().cloned());
        slots.extend(right.keys().cloned());
        let mut next = BTreeMap::new();
        for slot in slots {
            let left = current
                .get(&slot)
                .cloned()
                .unwrap_or(VirtualValue::Known(None));
            let right = VirtualValue::Known(right.get(&slot).cloned());
            let value = if left == right {
                left
            } else {
                VirtualValue::Unknown
            };
            if value != VirtualValue::Known(None) {
                next.insert(slot, value);
            }
        }
        current = next;
    }
    Ok(current)
}

fn virtual_merge_commits(
    connection: &Connection,
    ours_commit: HashId,
    theirs_commit: HashId,
) -> QueryResult<BTreeMap<String, VirtualValue>> {
    let bases = storage::best_common_ancestors(connection, ours_commit, theirs_commit)?;
    let base = if bases.is_empty() {
        BTreeMap::new()
    } else {
        virtual_base(connection, &bases)?
    };
    let ours = logical_slots(
        connection,
        &storage::load_snapshot_state(connection, ours_commit)?,
    )?;
    let theirs = logical_slots(
        connection,
        &storage::load_snapshot_state(connection, theirs_commit)?,
    )?;
    let mut slots = BTreeSet::new();
    slots.extend(base.keys().cloned());
    slots.extend(ours.keys().cloned());
    slots.extend(theirs.keys().cloned());
    let mut merged = BTreeMap::new();
    for slot in slots {
        let ours = ours.get(&slot).cloned();
        let theirs = theirs.get(&slot).cloned();
        let base = base
            .get(&slot)
            .cloned()
            .unwrap_or(VirtualValue::Known(None));
        let value = if same_optional_value(ours.as_ref(), theirs.as_ref()) {
            VirtualValue::Known(ours)
        } else {
            match base {
                VirtualValue::Known(base) if same_optional_value(ours.as_ref(), base.as_ref()) => {
                    VirtualValue::Known(theirs)
                }
                VirtualValue::Known(base)
                    if same_optional_value(theirs.as_ref(), base.as_ref()) =>
                {
                    VirtualValue::Known(ours)
                }
                _ => VirtualValue::Unknown,
            }
        };
        if value != VirtualValue::Known(None) {
            merged.insert(slot, value);
        }
    }
    Ok(merged)
}

fn conflict(
    identity: &MergeIdentity<'_>,
    slot: String,
    base: Option<Value>,
    ours: Option<Value>,
    theirs: Option<Value>,
) -> QueryResult<MergeConflict> {
    let id = conflict_id(
        &identity.base_identity,
        &identity.ours_identity,
        &identity.theirs_identity,
        &slot,
        base.as_ref(),
        ours.as_ref(),
        theirs.as_ref(),
    );
    let resolution = identity.resolutions.get(&id).cloned();
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

fn resolved_value(
    conflict: &MergeConflict,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<Option<Value>> {
    let Some(resolution) = resolutions.get(&conflict.id) else {
        return Ok(conflict.ours.clone());
    };
    let Value::Map(map) = resolution else {
        return Err(QueryError::invalid_argument(
            "merge resolution must be a Map",
        ));
    };
    match string_map(map, "choice")? {
        "ours" => Ok(conflict.ours.clone()),
        "theirs" => Ok(conflict.theirs.clone()),
        "value" => match map.get("value") {
            Some(Value::Null) | None => Ok(None),
            Some(value) => Ok(Some(value.clone())),
        },
        _ => Err(QueryError::invalid_argument(
            "resolution choice must be ours, theirs, or value",
        )),
    }
}

fn resolution_ids(value: Option<&Value>) -> QueryResult<BTreeSet<HashId>> {
    let mut ids = BTreeSet::new();
    for item in resolution_list(value)? {
        let map = resolution_map(item)?;
        let id = resolution_conflict_id(map)?;
        if !ids.insert(id) {
            return Err(QueryError::invalid_argument(
                "duplicate conflictId in resolutions",
            ));
        }
    }
    Ok(ids)
}

fn resolution_items(
    value: Option<&Value>,
    conflicts: &BTreeMap<HashId, MergeConflict>,
) -> QueryResult<BTreeMap<HashId, Value>> {
    let mut result = BTreeMap::new();
    for item in resolution_list(value)? {
        let map = resolution_map(item)?;
        let id = resolution_conflict_id(map)?;
        if result.contains_key(&id) {
            return Err(QueryError::invalid_argument(
                "duplicate conflictId in resolutions",
            ));
        }
        let conflict = conflicts
            .get(&id)
            .ok_or_else(|| QueryError::invalid_argument("unknown conflictId"))?;
        let choice = string_map(map, "choice")?;
        let mut normalized =
            BTreeMap::from([("choice".to_owned(), Value::String(choice.to_owned()))]);
        match choice {
            "ours" | "theirs" => {
                if map.contains_key("value") {
                    return Err(QueryError::invalid_argument(
                        "ours/theirs resolution cannot contain value",
                    ));
                }
            }
            "value" => {
                let value = map.get("value").ok_or_else(|| {
                    QueryError::invalid_argument("value resolution requires value")
                })?;
                validate_explicit_value(&conflict.slot, value)?;
                normalized.insert("value".to_owned(), value.clone());
            }
            _ => {
                return Err(QueryError::invalid_argument(
                    "resolution choice must be ours, theirs, or value",
                ));
            }
        }
        result.insert(id, Value::Map(normalized));
    }
    Ok(result)
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

fn encode_resolution(value: &Value) -> QueryResult<String> {
    serde_json::to_string(&cypher::encode_json(value)).map_err(|error| {
        QueryError::internal(format!("failed to encode merge resolution: {error}"))
    })
}

pub(super) fn load_resolutions(
    connection: &Connection,
    session: &str,
) -> QueryResult<BTreeMap<HashId, Value>> {
    storage::load_merge_resolutions(connection, session)?
        .into_iter()
        .map(|(id, text)| {
            let json: serde_json::Value = serde_json::from_str(&text).map_err(|_| {
                QueryError::new(
                    QueryErrorKind::Storage,
                    "stored merge resolution JSON is invalid",
                )
            })?;
            let value = cypher::decode_json(&json)?;
            Ok((id, value))
        })
        .collect()
}

fn session_status_row(
    session: &MergeSessionRecord,
    computation: &MergeComputation,
) -> ProcedureRow {
    row([
        ("session", Value::String(session.id.clone())),
        ("targetBranch", Value::String(session.target_branch.clone())),
        ("ours", commit_value(session.ours)),
        ("theirs", commit_value(session.theirs)),
        ("revision", Value::Integer(session.revision)),
        ("status", Value::String(computation.status.to_owned())),
        (
            "unresolved",
            Value::Integer(i64::try_from(computation.unresolved).unwrap_or(i64::MAX)),
        ),
    ])
}
