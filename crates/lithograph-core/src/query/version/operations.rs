use std::collections::{BTreeMap, BTreeSet};

use super::{
    VersionExecution as Connection, VersionOwned, VersionRows, VersionValue, VersionValues,
};
use crate::query::resources::{
    MemoryReservation, allocation_bytes, checked_add, checked_mul, map_entry_bytes,
};

use crate::cypher::Value;
use crate::storage::{self, CommitMetadata, HashId};

use super::{
    ProcedureRow, commit_value, ensure_target_branch_head, map_target_branch_error,
    move_target_branch, now_micros, resolve_descriptor, resolve_descriptor_at, row, string_arg,
    target_branch,
};
use crate::query::options::ExecutionOptions;
use crate::query::{QueryError, QueryErrorKind, QueryResult};

pub(super) fn squash(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_head: HashId,
) -> QueryResult<VersionRows> {
    let since = resolve_descriptor_at(connection, &args, 0, "since")?;
    let branch = target_branch(connection, options)?;
    let head = pinned_head;
    ensure_target_branch_head(connection, &branch, head)?;
    validate_squash_origin(connection, since, head)?;
    let target = connection.load_state(head)?;
    let base = connection.load_state(since)?;
    let layer = connection.layer_between(&base, &target)?;
    super::access::check_schema_validation(
        connection,
        &base.schema,
        &target.schema,
        &options.access,
    )?;
    super::patch::validate_candidate(connection, since, &layer, &target.schema)?;

    // Move inside the enclosing savepoint, then install the one replacement Commit.
    move_target_branch(connection, &branch, head, since)?;
    let commit = commit_rewritten_snapshot(connection, &branch, since, &layer, &target, options)?;
    let memory = connection.reserve(row_memory(3)?)?;
    Ok(VersionRows {
        value: vec![row([
            ("from", commit_value(since)),
            ("previousHead", commit_value(head)),
            ("commit", commit_value(commit)),
        ])],
        memory,
    })
}

fn validate_squash_origin(connection: &Connection, since: HashId, head: HashId) -> QueryResult<()> {
    if since == head {
        return Err(QueryError::invalid_argument(
            "squash since must be older than HEAD",
        ));
    }
    if !storage::is_ancestor(connection, since, head)? {
        return Err(QueryError::invalid_argument(
            "squash since must be an ancestor of HEAD",
        ));
    }
    Ok(())
}

pub(super) fn reset(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_head: HashId,
) -> QueryResult<VersionRows> {
    let target = resolve_descriptor_at(connection, &args, 0, "target")?;
    let branch = target_branch(connection, options)?;
    let previous = pinned_head;
    ensure_target_branch_head(connection, &branch, previous)?;
    super::access::check_commits(connection, previous, target, &options.access)?;
    move_target_branch(connection, &branch, previous, target)?;
    let memory = connection.reserve(row_memory(2)?)?;
    Ok(VersionRows {
        value: vec![row([
            ("from", commit_value(previous)),
            ("to", commit_value(target)),
        ])],
        memory,
    })
}

pub(super) fn revert(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_head: HashId,
) -> QueryResult<VersionRows> {
    let (commit, parent) = revert_target(connection, &args)?;
    let inverse = super::patch::diff_commits(connection, commit, parent)?;
    let new_commit = apply_revert_patch(connection, &inverse, options, pinned_head)?;
    let memory = connection.reserve(row_memory(1)?)?;
    Ok(VersionRows {
        value: vec![row([("commit", commit_value(new_commit))])],
        memory,
    })
}

fn revert_target(connection: &Connection, args: &[Value]) -> QueryResult<(HashId, HashId)> {
    let descriptor = string_arg(args, 0, "commit")?;
    if !descriptor.starts_with("commit/") {
        return Err(QueryError::invalid_argument(
            "revert commit must use commit/<id>",
        ));
    }
    let commit = resolve_descriptor(connection, &args[0], "commit")?;
    let (parent1, parent2) = commit_parents(connection, commit)?;
    if parent2.is_none() && args.get(1).is_some() {
        return Err(QueryError::invalid_argument(
            "revert options.mainline is only valid for Merge Commits",
        ));
    }
    let parent = match (parent1, parent2) {
        (None, _) => {
            return Err(QueryError::invalid_argument(
                "Root Commit cannot be reverted",
            ));
        }
        (Some(parent), None) => parent,
        (Some(parent1), Some(_parent2)) if revert_mainline(args.get(1))? == 1 => parent1,
        (Some(_), Some(parent2)) => parent2,
    };
    Ok((commit, parent))
}

fn apply_revert_patch(
    connection: &Connection,
    inverse: &Value,
    options: &ExecutionOptions,
    pinned_head: HashId,
) -> QueryResult<HashId> {
    let branch = target_branch(connection, options)?;
    let head = pinned_head;
    ensure_target_branch_head(connection, &branch, head)?;
    let base = connection.load_state(head)?;
    let mut target = connection.clone_state(&base)?;
    super::patch::apply_patch_to_state(connection, &mut target, inverse)?;
    let layer = connection.layer_between(&base, &target)?;
    super::access::check_layer(connection, head, &layer, &target.schema, &options.access)?;
    super::access::check_schema_validation(
        connection,
        &base.schema,
        &target.schema,
        &options.access,
    )?;
    super::patch::validate_candidate(connection, head, &layer, &target.schema)?;
    commit_rewritten_snapshot(connection, &branch, head, &layer, &target, options)
}

fn commit_rewritten_snapshot(
    connection: &Connection,
    branch: &str,
    parent: HashId,
    layer: &storage::LayerBuilder,
    target: &storage::SnapshotState,
    options: &ExecutionOptions,
) -> QueryResult<HashId> {
    let _persist_memory = snapshot_write_memory(connection, target)?;
    let schema_blob = target
        .schema
        .canonical_blob_checked(&super::execution::storage_check(connection.control()))?;
    let schema_hash = storage::persist_schema_blob(connection, &schema_blob)?;
    let metadata = copy_commit_metadata(connection, options)?;
    connection
        .commit_layer_with_schema(branch, parent, None, layer, schema_hash, &metadata)
        .map_err(|error| map_target_branch_error(error, branch))
}

pub(super) fn rebase(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_head: HashId,
) -> QueryResult<VersionRows> {
    let onto = resolve_descriptor_at(connection, &args, 0, "onto")?;
    let supplied_resolutions = rebase_resolutions(connection, args.get(1))?;
    let branch = target_branch(connection, options)?;
    let old_head = pinned_head;
    ensure_target_branch_head(connection, &branch, old_head)?;
    if storage::is_ancestor(connection, old_head, onto)? {
        return finish_up_to_date_rebase(
            connection,
            &branch,
            old_head,
            onto,
            &supplied_resolutions,
            options,
        );
    }
    let base = first_parent_rebase_base(connection, old_head, onto)?;
    let sequence = first_parent_sequence(connection, base, old_head)?;
    if sequence.is_empty() {
        return finish_up_to_date_rebase(
            connection,
            &branch,
            old_head,
            onto,
            &supplied_resolutions,
            options,
        );
    }
    execute_rebase_sequence(
        connection,
        &branch,
        old_head,
        onto,
        sequence,
        &supplied_resolutions,
        options,
    )
}

fn finish_up_to_date_rebase(
    connection: &Connection,
    branch: &str,
    old_head: HashId,
    onto: HashId,
    supplied_resolutions: &BTreeMap<HashId, Value>,
    options: &ExecutionOptions,
) -> QueryResult<VersionRows> {
    ensure_rebase_resolutions_known(supplied_resolutions, &BTreeSet::new())?;
    super::access::check_commits(connection, old_head, onto, &options.access)?;
    move_target_branch(connection, branch, old_head, onto)?;
    rebase_result(
        connection,
        "up_to_date",
        Some(onto),
        empty_values(connection)?,
        empty_values(connection)?,
    )
}

fn first_parent_rebase_base(
    connection: &Connection,
    head: HashId,
    onto: HashId,
) -> QueryResult<HashId> {
    let onto_ancestors = reachable_commits(connection, onto)?;
    let mut current = head;
    loop {
        if onto_ancestors.contains(&current) {
            return Ok(current);
        }
        connection.checkpoint()?;
        current = commit_parents(connection, current)?.0.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                "rebase inputs have no common Commit on the active first-parent chain",
            )
        })?;
    }
}

fn execute_rebase_sequence(
    connection: &Connection,
    branch: &str,
    old_head: HashId,
    onto: HashId,
    sequence: VersionOwned<Vec<HashId>>,
    supplied_resolutions: &BTreeMap<HashId, Value>,
    options: &ExecutionOptions,
) -> QueryResult<VersionRows> {
    connection.execute_batch("SAVEPOINT lithograph_rebase")?;
    move_target_branch(connection, branch, old_head, onto)?;
    match replay_rebase_sequence(
        connection,
        branch,
        onto,
        sequence,
        supplied_resolutions,
        options,
    )? {
        RebaseSequence::Conflict(conflicts) => {
            connection.execute_batch("ROLLBACK TO lithograph_rebase; RELEASE lithograph_rebase")?;
            check_rebase_conflict_access(connection, &conflicts, options)?;
            rebase_result(
                connection,
                "conflicted",
                None,
                empty_values(connection)?,
                conflicts,
            )
        }
        RebaseSequence::Applied { head, rewritten } => {
            super::access::check_commits(connection, old_head, head, &options.access)?;
            connection.execute_batch("RELEASE lithograph_rebase")?;
            rebase_result(
                connection,
                "rebased",
                Some(head),
                rewritten,
                empty_values(connection)?,
            )
        }
    }
}

enum RebaseSequence {
    Conflict(VersionValues),
    Applied {
        head: HashId,
        rewritten: VersionValues,
    },
}

fn replay_rebase_sequence(
    connection: &Connection,
    branch: &str,
    onto: HashId,
    sequence: VersionOwned<Vec<HashId>>,
    supplied_resolutions: &BTreeMap<HashId, Value>,
    options: &ExecutionOptions,
) -> QueryResult<RebaseSequence> {
    let mut replay_head = onto;
    let memory = connection.reserve(allocation_bytes::<Value>(sequence.len())?)?;
    let mut rewritten = VersionValues {
        value: Vec::with_capacity(sequence.len()),
        memory,
    };
    let mut seen_conflicts = VersionOwned {
        value: BTreeSet::new(),
        memory: connection.reserve(0)?,
    };
    for source in sequence.iter().copied() {
        connection.checkpoint()?;
        match replay_rebase_commit(
            connection,
            branch,
            replay_head,
            source,
            supplied_resolutions,
            options,
        )? {
            RebaseStep::Conflict {
                conflicts,
                seen_conflicts: step_conflicts,
            } => {
                extend_seen_conflicts(&mut seen_conflicts, step_conflicts)?;
                ensure_rebase_resolutions_known(supplied_resolutions, &seen_conflicts)?;
                return Ok(RebaseSequence::Conflict(conflicts));
            }
            RebaseStep::Applied {
                next,
                mapping,
                seen_conflicts: step_conflicts,
            } => {
                extend_seen_conflicts(&mut seen_conflicts, step_conflicts)?;
                let (mapping, memory) = mapping.into_parts();
                rewritten.memory.merge(memory)?;
                rewritten.push(mapping);
                replay_head = next;
            }
        }
    }
    ensure_rebase_resolutions_known(supplied_resolutions, &seen_conflicts)?;
    Ok(RebaseSequence::Applied {
        head: replay_head,
        rewritten,
    })
}

fn extend_seen_conflicts(
    seen: &mut VersionOwned<BTreeSet<HashId>>,
    additional: VersionOwned<BTreeSet<HashId>>,
) -> QueryResult<()> {
    seen.memory.grow(checked_mul(
        additional.len() as u64,
        map_entry_bytes::<HashId>()?,
    )?)?;
    let (additional, _additional_memory) = additional.into_parts();
    seen.extend(additional);
    Ok(())
}

fn check_rebase_conflict_access(
    connection: &Connection,
    conflicts: &[Value],
    options: &ExecutionOptions,
) -> QueryResult<()> {
    for conflict in conflicts {
        connection.checkpoint()?;
        if let Value::Map(conflict) = conflict
            && let Some(Value::String(slot)) = conflict.get("slot")
        {
            super::access::check_slot_read(slot, &options.access.read.query)?;
        }
    }
    Ok(())
}

enum RebaseStep {
    Conflict {
        conflicts: VersionValues,
        seen_conflicts: VersionOwned<BTreeSet<HashId>>,
    },
    Applied {
        next: HashId,
        mapping: VersionValue,
        seen_conflicts: VersionOwned<BTreeSet<HashId>>,
    },
}

fn replay_rebase_commit(
    connection: &Connection,
    branch: &str,
    replay_head: HashId,
    source: HashId,
    supplied_resolutions: &BTreeMap<HashId, Value>,
    options: &ExecutionOptions,
) -> QueryResult<RebaseStep> {
    let _source_record_memory = commit_metadata_memory(connection, source)?;
    let source_record = storage::load_commit(connection, source)?;
    let source_parent = source_record.parent1.ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::Storage,
            "rebase replay unexpectedly reached Root Commit",
        )
    })?;
    let outcome = super::merge::three_way_from_base(
        connection,
        source_parent,
        replay_head,
        source,
        supplied_resolutions,
    )?;
    let (conflicts, seen_conflicts) = rebase_conflicts(connection, &outcome.conflicts, source)?;
    if !conflicts.is_empty() {
        return Ok(RebaseStep::Conflict {
            conflicts,
            seen_conflicts,
        });
    }
    let next = apply_rebase_candidate(
        connection,
        branch,
        replay_head,
        &outcome.candidate,
        source_record.metadata,
        options,
    )?;
    Ok(RebaseStep::Applied {
        next,
        mapping: rebase_mapping(connection, source, next)?,
        seen_conflicts,
    })
}

fn rebase_conflicts(
    connection: &Connection,
    source_conflicts: &[super::merge::MergeConflict],
    source: HashId,
) -> QueryResult<(VersionValues, VersionOwned<BTreeSet<HashId>>)> {
    let memory = connection.reserve(allocation_bytes::<Value>(source_conflicts.len())?)?;
    let mut conflicts = VersionValues {
        value: Vec::with_capacity(source_conflicts.len()),
        memory,
    };
    let mut seen_conflicts = VersionOwned {
        value: BTreeSet::new(),
        memory: connection.reserve(checked_mul(
            source_conflicts.len() as u64,
            map_entry_bytes::<HashId>()?,
        )?)?,
    };
    for conflict in source_conflicts {
        connection.checkpoint()?;
        seen_conflicts.insert(conflict.id);
        if conflict.resolution.is_none() {
            let (value, memory) = rebase_conflict_value(connection, conflict, source)?.into_parts();
            conflicts.memory.merge(memory)?;
            conflicts.push(value);
        }
    }
    Ok((conflicts, seen_conflicts))
}

fn apply_rebase_candidate(
    connection: &Connection,
    branch: &str,
    replay_head: HashId,
    candidate: &storage::SnapshotState,
    metadata: CommitMetadata,
    options: &ExecutionOptions,
) -> QueryResult<HashId> {
    let current = connection.load_state(replay_head)?;
    let layer = connection.layer_between(&current, candidate)?;
    super::access::check_schema_validation(
        connection,
        &current.schema,
        &candidate.schema,
        &options.access,
    )?;
    super::patch::validate_candidate(connection, replay_head, &layer, &candidate.schema)?;
    let _persist_memory = snapshot_write_memory(connection, candidate)?;
    let schema_blob = candidate
        .schema
        .canonical_blob_checked(&super::execution::storage_check(connection.control()))?;
    let schema_hash = storage::persist_schema_blob(connection, &schema_blob)?;
    connection
        .commit_layer_with_schema(
            branch,
            replay_head,
            None,
            &layer,
            schema_hash,
            &CommitMetadata {
                author: metadata.author,
                message: metadata.message,
                committed_at: now_micros()?,
            },
        )
        .map_err(Into::into)
}

fn rebase_mapping(
    connection: &Connection,
    source: HashId,
    next: HashId,
) -> QueryResult<VersionValue> {
    let memory = connection.reserve(checked_add(
        checked_mul(map_entry_bytes::<Value>()?, 2)?,
        192,
    )?)?;
    Ok(VersionValue {
        value: Value::Map(BTreeMap::from([
            ("from".to_owned(), commit_value(source)),
            ("to".to_owned(), commit_value(next)),
        ])),
        memory,
    })
}

fn rebase_result(
    connection: &Connection,
    status: &str,
    commit: Option<HashId>,
    rewritten: VersionValues,
    conflicts: VersionValues,
) -> QueryResult<VersionRows> {
    connection.checkpoint()?;
    let (rewritten, mut memory) = rewritten.into_parts();
    let (conflicts, conflicts_memory) = conflicts.into_parts();
    memory.merge(conflicts_memory)?;
    memory.grow(checked_add(row_memory(4)?, status.len() as u64)?)?;
    Ok(VersionRows {
        value: vec![row([
            ("status", Value::String(status.to_owned())),
            ("commit", commit.map(commit_value).unwrap_or(Value::Null)),
            ("rewritten", Value::List(rewritten)),
            ("conflicts", Value::List(conflicts)),
        ])],
        memory,
    })
}

fn empty_values(connection: &Connection) -> QueryResult<VersionValues> {
    Ok(VersionValues {
        value: Vec::new(),
        memory: connection.reserve(0)?,
    })
}

fn row_memory(fields: u64) -> QueryResult<u64> {
    checked_add(
        allocation_bytes::<ProcedureRow>(1)?,
        checked_add(
            checked_mul(map_entry_bytes::<Value>()?, fields)?,
            checked_mul(fields, 128)?,
        )?,
    )
}

pub(super) fn gc(connection: &Connection) -> QueryResult<VersionRows> {
    let counters = storage::collect_garbage(connection)?;
    let memory = connection.reserve(row_memory(5)?)?;
    Ok(VersionRows {
        value: vec![row([
            ("commits", count_value(counters.commits)),
            ("layers", count_value(counters.layers)),
            ("schemas", count_value(counters.schemas)),
            ("checkpoints", count_value(counters.checkpoints)),
            ("commitData", count_value(counters.commit_data)),
        ])],
        memory,
    })
}

fn rebase_conflict_value(
    connection: &Connection,
    conflict: &super::merge::MergeConflict,
    source: HashId,
) -> QueryResult<VersionValue> {
    let mut memory = connection.reserve(checked_add(
        checked_mul(map_entry_bytes::<Value>()?, 6)?,
        checked_add(conflict.slot.len() as u64, 256)?,
    )?)?;
    let mut copy = |value: Option<&Value>| -> QueryResult<Value> {
        match value {
            Some(value) => {
                let (value, value_memory) = connection.copy_value(value)?;
                memory.merge(value_memory)?;
                Ok(value)
            }
            None => Ok(Value::Null),
        }
    };
    let base = copy(conflict.base.as_ref())?;
    let ours = copy(conflict.ours.as_ref())?;
    let theirs = copy(conflict.theirs.as_ref())?;
    Ok(VersionValue {
        value: Value::Map(BTreeMap::from([
            ("conflictId".to_owned(), Value::String(conflict.id.to_hex())),
            ("slot".to_owned(), Value::String(conflict.slot.clone())),
            ("base".to_owned(), base),
            ("ours".to_owned(), ours),
            ("theirs".to_owned(), theirs),
            ("sourceCommit".to_owned(), commit_value(source)),
        ])),
        memory,
    })
}

fn rebase_resolutions(
    connection: &Connection,
    value: Option<&Value>,
) -> QueryResult<VersionOwned<BTreeMap<HashId, Value>>> {
    let mut output = VersionOwned {
        value: BTreeMap::new(),
        memory: connection.reserve(0)?,
    };
    let Some(value) = value else {
        return Ok(output);
    };
    for item in rebase_resolution_items(value)? {
        connection.checkpoint()?;
        let (id, normalized) = normalize_rebase_resolution(connection, item)?;
        output.memory.grow(map_entry_bytes::<Value>()?)?;
        let (normalized, memory) = normalized.into_parts();
        output.memory.merge(memory)?;
        if output.insert(id, normalized).is_some() {
            return Err(QueryError::invalid_argument("duplicate rebase conflictId"));
        }
    }
    Ok(output)
}

fn rebase_resolution_items(value: &Value) -> QueryResult<&[Value]> {
    let Value::Map(options) = value else {
        return Err(QueryError::invalid_argument("rebase options must be a Map"));
    };
    if options.keys().any(|key| key != "resolutions") {
        return Err(QueryError::invalid_argument(
            "rebase options only support resolutions",
        ));
    }
    match options.get("resolutions") {
        Some(Value::List(items)) => Ok(items),
        _ => Err(QueryError::invalid_argument(
            "rebase options.resolutions must be a List",
        )),
    }
}

fn rebase_resolution_fields(
    value: &Value,
) -> QueryResult<(&BTreeMap<String, Value>, HashId, &str)> {
    let Value::Map(map) = value else {
        return Err(QueryError::invalid_argument(
            "rebase resolution must be a Map",
        ));
    };
    let id = match map.get("conflictId") {
        Some(Value::String(id)) => HashId::from_hex(id)
            .map_err(|_| QueryError::invalid_argument("invalid rebase conflictId"))?,
        _ => {
            return Err(QueryError::invalid_argument(
                "rebase resolution needs conflictId",
            ));
        }
    };
    if map
        .keys()
        .any(|key| !matches!(key.as_str(), "conflictId" | "choice" | "value"))
    {
        return Err(QueryError::invalid_argument(
            "rebase resolution contains an unknown field",
        ));
    }
    let choice = match map.get("choice") {
        Some(Value::String(choice)) => choice.as_str(),
        _ => {
            return Err(QueryError::invalid_argument(
                "rebase resolution needs a String choice",
            ));
        }
    };
    Ok((map, id, choice))
}

fn normalize_rebase_resolution(
    connection: &Connection,
    item: &Value,
) -> QueryResult<(HashId, VersionValue)> {
    let (map, id, choice) = rebase_resolution_fields(item)?;
    let mut memory = connection.reserve(checked_add(
        checked_mul(map_entry_bytes::<Value>()?, 2)?,
        checked_add(choice.len() as u64, 64)?,
    )?)?;
    let mut normalized = BTreeMap::from([("choice".to_owned(), Value::String(choice.to_owned()))]);
    match choice {
        "ours" | "theirs" => {
            if map.contains_key("value") {
                return Err(QueryError::invalid_argument(
                    "ours/theirs rebase resolution cannot contain value",
                ));
            }
        }
        "value" => {
            let value = map.get("value").ok_or_else(|| {
                QueryError::invalid_argument("value rebase resolution requires value")
            })?;
            let (value, value_memory) = connection.copy_value(value)?;
            memory.merge(value_memory)?;
            normalized.insert("value".to_owned(), value);
        }
        _ => {
            return Err(QueryError::invalid_argument(
                "rebase resolution choice must be ours, theirs, or value",
            ));
        }
    }
    Ok((
        id,
        VersionValue {
            value: Value::Map(normalized),
            memory,
        },
    ))
}

fn ensure_rebase_resolutions_known(
    resolutions: &BTreeMap<HashId, Value>,
    seen_conflicts: &BTreeSet<HashId>,
) -> QueryResult<()> {
    if resolutions
        .keys()
        .any(|conflict_id| !seen_conflicts.contains(conflict_id))
    {
        return Err(QueryError::invalid_argument("unknown rebase conflictId"));
    }
    Ok(())
}

fn first_parent_sequence(
    connection: &Connection,
    base: HashId,
    head: HashId,
) -> QueryResult<VersionOwned<Vec<HashId>>> {
    let mut reversed = VersionOwned {
        value: Vec::new(),
        memory: connection.reserve(0)?,
    };
    let mut current = head;
    while current != base {
        connection.checkpoint()?;
        if reversed.len() == reversed.capacity() {
            let capacity = reversed.capacity().max(4).checked_mul(2).ok_or_else(|| {
                crate::query::resources::memory_error("rebase sequence size overflow")
            })?;
            let additional = capacity - reversed.capacity();
            reversed
                .memory
                .grow(allocation_bytes::<HashId>(additional)?)?;
            let additional = capacity - reversed.len();
            reversed.value.try_reserve_exact(additional).map_err(|_| {
                crate::query::resources::memory_error("cannot allocate rebase sequence")
            })?;
        }
        reversed.push(current);
        connection.checkpoint()?;
        current = commit_parents(connection, current)?.0.ok_or_else(|| {
            QueryError::invalid_argument(
                "merge-base is not on the active Branch first-parent chain",
            )
        })?;
    }
    reversed.reverse();
    Ok(reversed)
}

fn revert_mainline(value: Option<&Value>) -> QueryResult<i64> {
    let Some(Value::Map(options)) = value else {
        return Err(QueryError::invalid_argument(
            "Merge Commit revert requires options.mainline",
        ));
    };
    if options.keys().any(|key| key != "mainline") {
        return Err(QueryError::invalid_argument(
            "revert options only support mainline",
        ));
    }
    match options.get("mainline") {
        Some(Value::Integer(value @ (1 | 2))) => Ok(*value),
        _ => Err(QueryError::invalid_argument(
            "revert options.mainline must be 1 or 2",
        )),
    }
}

fn count_value(value: usize) -> Value {
    Value::Integer(i64::try_from(value).unwrap_or(i64::MAX))
}

/// Parent-only reads avoid copying arbitrary author/message text during DAG walks.
pub(super) fn commit_parents(
    connection: &Connection,
    commit: HashId,
) -> QueryResult<(Option<HashId>, Option<HashId>)> {
    connection.checkpoint()?;
    let _temporary = connection.reserve(128)?;
    let (parent1, parent2) = connection.query_row(
        "SELECT parent1, parent2 FROM main._lithograph_commits WHERE id = ?1",
        [commit.as_bytes().as_slice()],
        |row| {
            Ok((
                row.get::<_, Option<Vec<u8>>>(0)?,
                row.get::<_, Option<Vec<u8>>>(1)?,
            ))
        },
    )?;
    Ok((
        parent1.as_deref().map(HashId::from_slice).transpose()?,
        parent2.as_deref().map(HashId::from_slice).transpose()?,
    ))
}

fn reachable_commits(
    connection: &Connection,
    root: HashId,
) -> QueryResult<VersionOwned<BTreeSet<HashId>>> {
    let mut output = VersionOwned {
        value: BTreeSet::new(),
        memory: connection.reserve(0)?,
    };
    let mut pending = BTreeSet::new();
    let mut pending_memory = connection.reserve(map_entry_bytes::<HashId>()?)?;
    pending.insert(root);
    while let Some(commit) = pending.pop_first() {
        connection.checkpoint()?;
        if output.contains(&commit) {
            continue;
        }
        output.memory.grow(map_entry_bytes::<HashId>()?)?;
        output.insert(commit);
        let (parent1, parent2) = commit_parents(connection, commit)?;
        for parent in [parent1, parent2].into_iter().flatten() {
            if !output.contains(&parent) && !pending.contains(&parent) {
                pending_memory.grow(map_entry_bytes::<HashId>()?)?;
                pending.insert(parent);
            }
        }
    }
    Ok(output)
}

fn commit_metadata_memory(
    connection: &Connection,
    commit: HashId,
) -> QueryResult<MemoryReservation> {
    let bytes: i64 = connection.query_row(
        "SELECT coalesce(length(CAST(author AS BLOB)), 0) + coalesce(length(CAST(message AS BLOB)), 0) FROM main._lithograph_commits WHERE id = ?1",
        [commit.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    let bytes = u64::try_from(bytes)
        .map_err(|_| QueryError::internal("negative Commit metadata byte count"))?;
    connection.reserve(checked_add(bytes, 256)?)
}

fn snapshot_write_memory(
    connection: &Connection,
    target: &storage::SnapshotState,
) -> QueryResult<MemoryReservation> {
    let check = || {
        connection
            .control()
            .check()
            .map_err(super::execution::storage_error)
    };
    let state = storage::snapshot_state_memory_bytes(target, &check)?;
    connection.reserve(checked_mul(state, 8)?)
}

pub(super) fn copy_commit_metadata(
    connection: &Connection,
    options: &ExecutionOptions,
) -> QueryResult<VersionOwned<CommitMetadata>> {
    let bytes = [&options.author, &options.message]
        .into_iter()
        .flatten()
        .try_fold(0, |bytes, text| checked_add(bytes, text.len() as u64))?;
    let memory = connection.reserve(checked_add(bytes, 256)?)?;
    let copy = |text: &String| {
        crate::query::expression::clone_string_controlled(text, Some(connection.control()))
    };
    let author = options.author.as_ref().map(copy).transpose()?;
    let message = options.message.as_ref().map(copy).transpose()?;
    Ok(VersionOwned {
        value: CommitMetadata {
            author,
            message,
            committed_at: now_micros()?,
        },
        memory,
    })
}
