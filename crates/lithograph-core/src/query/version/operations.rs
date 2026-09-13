use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::Value;
use crate::storage::{self, CommitMetadata, HashId};

use super::{
    ProcedureRow, commit_value, now_micros, resolve_descriptor, resolve_descriptor_at, row,
    string_arg, target_branch,
};
use crate::query::options::ExecutionOptions;
use crate::query::{QueryError, QueryErrorKind, QueryResult};

pub(super) fn squash(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<Vec<ProcedureRow>> {
    let since = resolve_descriptor_at(connection, &args, 0, "since")?;
    let branch = target_branch(connection, options)?;
    let head = storage::branch_head(connection, &branch)?;
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
    let target = storage::load_snapshot_state(connection, head)?;
    let base = storage::load_snapshot_state(connection, since)?;
    let layer = storage::layer_between(&base, &target)?;
    super::patch::validate_candidate(connection, since, &layer, &target.schema)?;

    // Move inside the enclosing savepoint, then install the one replacement Commit.
    storage::move_branch_ref(connection, &branch, Some(head), since)?;
    let schema_hash = target.schema.persist(connection)?;
    let commit = storage::commit_layer_with_schema(
        connection,
        &branch,
        since,
        None,
        &layer,
        schema_hash,
        &CommitMetadata {
            author: options.author.clone(),
            message: options.message.clone(),
            committed_at: now_micros()?,
        },
    )?;
    Ok(vec![row([
        ("from", commit_value(since)),
        ("previousHead", commit_value(head)),
        ("commit", commit_value(commit)),
    ])])
}

pub(super) fn reset(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<Vec<ProcedureRow>> {
    let target = resolve_descriptor_at(connection, &args, 0, "target")?;
    let branch = target_branch(connection, options)?;
    let previous = storage::branch_head(connection, &branch)?;
    storage::move_branch_ref(connection, &branch, Some(previous), target)?;
    Ok(vec![row([
        ("from", commit_value(previous)),
        ("to", commit_value(target)),
    ])])
}

pub(super) fn revert(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<Vec<ProcedureRow>> {
    let (commit, parent) = revert_target(connection, &args)?;
    let inverse = super::patch::diff_commits(connection, commit, parent)?;
    let new_commit = apply_revert_patch(connection, &inverse, options)?;
    Ok(vec![row([("commit", commit_value(new_commit))])])
}

fn revert_target(connection: &Connection, args: &[Value]) -> QueryResult<(HashId, HashId)> {
    let descriptor = string_arg(args, 0, "commit")?;
    if !descriptor.starts_with("commit/") {
        return Err(QueryError::invalid_argument(
            "revert commit must use commit/<id>",
        ));
    }
    let commit = resolve_descriptor(connection, &args[0], "commit")?;
    let record = storage::load_commit(connection, commit)?;
    if record.parent2.is_none() && args.get(1).is_some() {
        return Err(QueryError::invalid_argument(
            "revert options.mainline is only valid for Merge Commits",
        ));
    }
    let parent = match (record.parent1, record.parent2) {
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
) -> QueryResult<HashId> {
    let branch = target_branch(connection, options)?;
    let head = storage::branch_head(connection, &branch)?;
    let base = storage::load_snapshot_state(connection, head)?;
    let mut target = base.clone();
    super::patch::apply_patch_to_state(connection, &mut target, inverse)?;
    let layer = storage::layer_between(&base, &target)?;
    super::patch::validate_candidate(connection, head, &layer, &target.schema)?;
    let schema_hash = target.schema.persist(connection)?;
    let new_commit = storage::commit_layer_with_schema(
        connection,
        &branch,
        head,
        None,
        &layer,
        schema_hash,
        &CommitMetadata {
            author: options.author.clone(),
            message: options.message.clone(),
            committed_at: now_micros()?,
        },
    )?;
    Ok(new_commit)
}

pub(super) fn rebase(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<Vec<ProcedureRow>> {
    let onto = resolve_descriptor_at(connection, &args, 0, "onto")?;
    let supplied_resolutions = rebase_resolutions(args.get(1))?;
    let branch = target_branch(connection, options)?;
    let old_head = storage::branch_head(connection, &branch)?;
    if storage::is_ancestor(connection, old_head, onto)? {
        ensure_rebase_resolutions_known(&supplied_resolutions, &BTreeSet::new())?;
        storage::move_branch_ref(connection, &branch, Some(old_head), onto)?;
        return Ok(rebase_result(
            "up_to_date",
            Some(onto),
            Vec::new(),
            Vec::new(),
        ));
    }
    let base = first_parent_rebase_base(connection, old_head, onto)?;
    let sequence = first_parent_sequence(connection, base, old_head)?;
    if sequence.is_empty() {
        ensure_rebase_resolutions_known(&supplied_resolutions, &BTreeSet::new())?;
        storage::move_branch_ref(connection, &branch, Some(old_head), onto)?;
        return Ok(rebase_result(
            "up_to_date",
            Some(onto),
            Vec::new(),
            Vec::new(),
        ));
    }
    execute_rebase_sequence(
        connection,
        &branch,
        old_head,
        onto,
        sequence,
        &supplied_resolutions,
    )
}

fn first_parent_rebase_base(
    connection: &Connection,
    head: HashId,
    onto: HashId,
) -> QueryResult<HashId> {
    let onto_ancestors = storage::reachable_commits(connection, [onto])?;
    let mut current = head;
    loop {
        if onto_ancestors.contains(&current) {
            return Ok(current);
        }
        let record = storage::load_commit(connection, current)?;
        current = record.parent1.ok_or_else(|| {
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
    sequence: Vec<HashId>,
    supplied_resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<Vec<ProcedureRow>> {
    connection.execute_batch("SAVEPOINT lithograph_rebase")?;
    storage::move_branch_ref(connection, branch, Some(old_head), onto)?;
    let mut replay_head = onto;
    let mut rewritten = Vec::new();
    let mut seen_conflicts = BTreeSet::new();
    for source in sequence {
        match replay_rebase_commit(
            connection,
            branch,
            replay_head,
            source,
            supplied_resolutions,
        )? {
            RebaseStep::Conflict {
                conflicts,
                seen_conflicts: step_conflicts,
            } => {
                seen_conflicts.extend(step_conflicts);
                ensure_rebase_resolutions_known(supplied_resolutions, &seen_conflicts)?;
                connection
                    .execute_batch("ROLLBACK TO lithograph_rebase; RELEASE lithograph_rebase")?;
                return Ok(rebase_result("conflicted", None, Vec::new(), conflicts));
            }
            RebaseStep::Applied {
                next,
                mapping,
                seen_conflicts: step_conflicts,
            } => {
                seen_conflicts.extend(step_conflicts);
                rewritten.push(mapping);
                replay_head = next;
            }
        }
    }
    ensure_rebase_resolutions_known(supplied_resolutions, &seen_conflicts)?;
    connection.execute_batch("RELEASE lithograph_rebase")?;
    Ok(rebase_result(
        "rebased",
        Some(replay_head),
        rewritten,
        Vec::new(),
    ))
}

enum RebaseStep {
    Conflict {
        conflicts: Vec<Value>,
        seen_conflicts: BTreeSet<HashId>,
    },
    Applied {
        next: HashId,
        mapping: Value,
        seen_conflicts: BTreeSet<HashId>,
    },
}

fn replay_rebase_commit(
    connection: &Connection,
    branch: &str,
    replay_head: HashId,
    source: HashId,
    supplied_resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<RebaseStep> {
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
    let conflicts = outcome
        .conflicts
        .iter()
        .filter(|conflict| conflict.resolution.is_none())
        .map(|conflict| rebase_conflict_value(conflict, source))
        .collect::<Vec<_>>();
    let seen_conflicts = outcome
        .conflicts
        .iter()
        .map(|conflict| conflict.id)
        .collect::<BTreeSet<_>>();
    if !conflicts.is_empty() {
        return Ok(RebaseStep::Conflict {
            conflicts,
            seen_conflicts,
        });
    }
    let current = storage::load_snapshot_state(connection, replay_head)?;
    let layer = storage::layer_between(&current, &outcome.candidate)?;
    super::patch::validate_candidate(connection, replay_head, &layer, &outcome.candidate.schema)?;
    let schema_hash = outcome.candidate.schema.persist(connection)?;
    let next = storage::commit_layer_with_schema(
        connection,
        branch,
        replay_head,
        None,
        &layer,
        schema_hash,
        &CommitMetadata {
            author: source_record.metadata.author,
            message: source_record.metadata.message,
            committed_at: now_micros()?,
        },
    )?;
    Ok(RebaseStep::Applied {
        next,
        mapping: Value::Map(BTreeMap::from([
            ("from".to_owned(), commit_value(source)),
            ("to".to_owned(), commit_value(next)),
        ])),
        seen_conflicts,
    })
}

fn rebase_result(
    status: &str,
    commit: Option<HashId>,
    rewritten: Vec<Value>,
    conflicts: Vec<Value>,
) -> Vec<ProcedureRow> {
    vec![row([
        ("status", Value::String(status.to_owned())),
        ("commit", commit.map(commit_value).unwrap_or(Value::Null)),
        ("rewritten", Value::List(rewritten)),
        ("conflicts", Value::List(conflicts)),
    ])]
}

pub(super) fn gc(connection: &Connection) -> QueryResult<Vec<ProcedureRow>> {
    let counters = storage::collect_garbage(connection)?;
    Ok(vec![row([
        ("commits", count_value(counters.commits)),
        ("layers", count_value(counters.layers)),
        ("schemas", count_value(counters.schemas)),
        ("checkpoints", count_value(counters.checkpoints)),
        ("commitData", count_value(counters.commit_data)),
    ])])
}

fn rebase_conflict_value(conflict: &super::merge::MergeConflict, source: HashId) -> Value {
    Value::Map(BTreeMap::from([
        ("conflictId".to_owned(), Value::String(conflict.id.to_hex())),
        ("slot".to_owned(), Value::String(conflict.slot.clone())),
        (
            "base".to_owned(),
            conflict.base.clone().unwrap_or(Value::Null),
        ),
        (
            "ours".to_owned(),
            conflict.ours.clone().unwrap_or(Value::Null),
        ),
        (
            "theirs".to_owned(),
            conflict.theirs.clone().unwrap_or(Value::Null),
        ),
        ("sourceCommit".to_owned(), commit_value(source)),
    ]))
}

fn rebase_resolutions(value: Option<&Value>) -> QueryResult<BTreeMap<HashId, Value>> {
    let Some(value) = value else {
        return Ok(BTreeMap::new());
    };
    let Value::Map(options) = value else {
        return Err(QueryError::invalid_argument("rebase options must be a Map"));
    };
    if options.keys().any(|key| key != "resolutions") {
        return Err(QueryError::invalid_argument(
            "rebase options only support resolutions",
        ));
    }
    let Some(Value::List(items)) = options.get("resolutions") else {
        return Err(QueryError::invalid_argument(
            "rebase options.resolutions must be a List",
        ));
    };
    let mut output = BTreeMap::new();
    for item in items {
        let Value::Map(map) = item else {
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
        let mut normalized =
            BTreeMap::from([("choice".to_owned(), Value::String(choice.to_owned()))]);
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
                normalized.insert("value".to_owned(), value.clone());
            }
            _ => {
                return Err(QueryError::invalid_argument(
                    "rebase resolution choice must be ours, theirs, or value",
                ));
            }
        }
        if output.insert(id, Value::Map(normalized)).is_some() {
            return Err(QueryError::invalid_argument("duplicate rebase conflictId"));
        }
    }
    Ok(output)
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
) -> QueryResult<Vec<HashId>> {
    let mut reversed = Vec::new();
    let mut current = head;
    while current != base {
        reversed.push(current);
        let record = storage::load_commit(connection, current)?;
        current = record.parent1.ok_or_else(|| {
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
