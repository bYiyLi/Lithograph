use std::cmp::Reverse;
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::Value;
use crate::storage::{self, CommitMetadata, HashId};

use super::options::{ExecutionOptions, MergeSessionSelector, SnapshotSelector};
use super::{QueryError, QueryErrorKind, QueryResult, now_micros};

mod merge;
mod operations;
mod patch;

pub(crate) type ProcedureRow = BTreeMap<String, Value>;

pub(crate) struct ProcedureOutcome {
    pub(crate) rows: Vec<ProcedureRow>,
    pub(crate) summary_commit: HashId,
}

#[derive(Debug)]
struct LogTraversal {
    start: HashId,
    ready: BTreeSet<(Reverse<i64>, HashId)>,
    pending_children: BTreeMap<HashId, usize>,
}

#[derive(Debug, Clone)]
pub(crate) struct CandidateContext {
    pub(crate) base_commit: HashId,
    pub(crate) layer: storage::LayerBuilder,
    pub(crate) schema: storage::SchemaState,
    pub(crate) session_id: String,
    pub(crate) revision: i64,
}

pub(crate) fn resolve_candidate_context(
    connection: &Connection,
    selector: &MergeSessionSelector,
) -> QueryResult<CandidateContext> {
    with_read_snapshot(connection, |connection| {
        resolve_candidate_context_snapshot(connection, selector)
    })
}

fn resolve_candidate_context_snapshot(
    connection: &Connection,
    selector: &MergeSessionSelector,
) -> QueryResult<CandidateContext> {
    let session = require_candidate_session(connection, selector)?;
    let resolutions = merge::load_resolutions(connection, &selector.id)?;
    if let Some(context) = sparse_candidate_context(connection, selector, &session, &resolutions)? {
        return Ok(context);
    }
    candidate_context_from_computation(connection, selector, &session, &resolutions)
}

fn require_candidate_session(
    connection: &Connection,
    selector: &MergeSessionSelector,
) -> QueryResult<storage::MergeSessionRecord> {
    let session = storage::load_merge_session(connection, &selector.id)?.ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::MergeSessionNotFound,
            format!("Merge Session {:?} was not found", selector.id),
        )
    })?;
    if session.revision != selector.revision {
        return Err(QueryError::new(
            QueryErrorKind::MergeSessionChanged,
            format!("Merge Session {:?} revision changed", selector.id),
        ));
    }
    Ok(session)
}

fn sparse_candidate_context(
    connection: &Connection,
    selector: &MergeSessionSelector,
    session: &storage::MergeSessionRecord,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<Option<CandidateContext>> {
    if let Some((layer, schema, unresolved)) =
        merge::sparse_candidate_layer(connection, session.ours, session.theirs, resolutions)?
    {
        if unresolved != 0 {
            return Err(QueryError::new(
                QueryErrorKind::MergeConflict,
                "Merge Session has unresolved conflicts",
            ));
        }
        return Ok(Some(CandidateContext {
            base_commit: session.ours,
            layer,
            schema,
            session_id: selector.id.clone(),
            revision: selector.revision,
        }));
    }
    Ok(None)
}

fn candidate_context_from_computation(
    connection: &Connection,
    selector: &MergeSessionSelector,
    session: &storage::MergeSessionRecord,
    resolutions: &BTreeMap<HashId, Value>,
) -> QueryResult<CandidateContext> {
    let computation =
        merge::compute_summary(connection, session.ours, session.theirs, resolutions)?;
    if computation.unresolved != 0 {
        return Err(QueryError::new(
            QueryErrorKind::MergeConflict,
            "Merge Session has unresolved conflicts",
        ));
    }
    let candidate = computation
        .candidate
        .ok_or_else(|| QueryError::internal("resolved Merge Session is missing candidate state"))?;
    let base_commit = match computation.status {
        "fast_forward" => session.theirs,
        "up_to_date" | "ready" => session.ours,
        status => {
            return Err(QueryError::internal(format!(
                "resolved Merge Session has unexpected status {status}"
            )));
        }
    };
    let base = storage::load_snapshot_state(connection, base_commit)?;
    let layer = storage::layer_between(&base, &candidate)?;
    Ok(CandidateContext {
        base_commit,
        layer,
        schema: candidate.schema,
        session_id: selector.id.clone(),
        revision: selector.revision,
    })
}

pub(super) fn with_read_snapshot<T>(
    connection: &Connection,
    operation: impl FnOnce(&Connection) -> QueryResult<T>,
) -> QueryResult<T> {
    if !connection.is_autocommit() {
        return operation(connection);
    }
    connection.execute_batch("SAVEPOINT lithograph_version_read_snapshot")?;
    let result = operation(connection);
    match result {
        Ok(value) => {
            connection.execute_batch("RELEASE lithograph_version_read_snapshot")?;
            Ok(value)
        }
        Err(error) => {
            connection.execute_batch(
                "ROLLBACK TO lithograph_version_read_snapshot; RELEASE lithograph_version_read_snapshot",
            )?;
            Err(error)
        }
    }
}

#[doc(hidden)]
pub fn validate_candidate_state(
    connection: &Connection,
    base_commit: HashId,
    state: &storage::SnapshotState,
) -> QueryResult<()> {
    let base = storage::load_snapshot_state(connection, base_commit)?;
    let layer = storage::layer_between(&base, state)?;
    patch::validate_candidate(connection, base_commit, &layer, &state.schema)
}

pub(crate) fn execute_procedure(
    connection: &Connection,
    name: &str,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_commit: HashId,
) -> QueryResult<ProcedureOutcome> {
    validate_options(name, options)?;
    let normalized = name.to_ascii_lowercase();
    let rows = match normalized.as_str() {
        "lithograph.branch.create" => branch_create(connection, args, pinned_commit),
        "lithograph.branch.checkout" => branch_checkout(connection, args),
        "lithograph.branch.list" => branch_list(connection),
        "lithograph.branch.delete" => branch_delete(connection, args),
        "lithograph.commit.get" => commit_get(connection, args),
        "lithograph.commit.create" => commit_create(connection, args, options, pinned_commit),
        "lithograph.commit.data.set" => commit_data_set(connection, args),
        "lithograph.commit.data.clear" => commit_data_clear(connection, args),
        "lithograph.tag.create" => tag_create(connection, args),
        "lithograph.tag.list" => tag_list(connection),
        "lithograph.tag.move" => tag_move(connection, args),
        "lithograph.tag.delete" => tag_delete(connection, args),
        "lithograph.log" => log(connection, args),
        "lithograph.diff" => patch::diff(connection, args),
        "lithograph.patch.apply" => patch::apply(connection, args, options, pinned_commit),
        "lithograph.merge.start" => merge::start(connection, args, options, pinned_commit),
        "lithograph.merge.get" => merge::get(connection, args),
        "lithograph.merge.list" => merge::list(connection, args),
        "lithograph.merge.conflicts" => merge::conflicts(connection, args),
        "lithograph.merge.resolve" => merge::resolve(connection, args),
        "lithograph.merge.finalize" => merge::finalize(connection, args, options),
        "lithograph.merge.abort" => merge::abort(connection, args),
        "lithograph.rebase" => operations::rebase(connection, args, options, pinned_commit),
        "lithograph.squash" => operations::squash(connection, args, options, pinned_commit),
        "lithograph.reset" => operations::reset(connection, args, options, pinned_commit),
        "lithograph.revert" => operations::revert(connection, args, options, pinned_commit),
        "lithograph.gc" => operations::gc(connection),
        _ => Err(QueryError::internal(format!(
            "Version Procedure {name} is registered but not implemented"
        ))),
    }?;
    let summary_commit = if super::registry::is_version_mutation(normalized.as_str()) {
        procedure_summary_commit(connection, normalized.as_str(), &rows)?
    } else {
        pinned_commit
    };
    Ok(ProcedureOutcome {
        rows,
        summary_commit,
    })
}

fn procedure_summary_commit(
    connection: &Connection,
    name: &str,
    rows: &[ProcedureRow],
) -> QueryResult<HashId> {
    let result_field = match name {
        "lithograph.commit.create"
        | "lithograph.patch.apply"
        | "lithograph.squash"
        | "lithograph.revert" => Some("commit"),
        "lithograph.merge.finalize" if procedure_status(rows)? == "merged" => Some("commit"),
        "lithograph.rebase" if procedure_status(rows)? == "rebased" => Some("commit"),
        _ => None,
    };
    if let Some(field) = result_field {
        if let Some(commit) = procedure_commit_field(rows, field)? {
            return Ok(commit);
        }
        return Err(QueryError::internal(format!(
            "Version Procedure {name} did not return its observable Commit"
        )));
    }
    active_head(connection)
}

fn procedure_status(rows: &[ProcedureRow]) -> QueryResult<&str> {
    match rows.last().and_then(|row| row.get("status")) {
        Some(Value::String(status)) => Ok(status),
        _ => Err(QueryError::internal(
            "Version Procedure result is missing status",
        )),
    }
}

fn procedure_commit_field(rows: &[ProcedureRow], field: &str) -> QueryResult<Option<HashId>> {
    let value = rows.last().and_then(|row| row.get(field)).ok_or_else(|| {
        QueryError::internal(format!("Version Procedure result is missing {field}"))
    })?;
    match value {
        Value::Null => Ok(None),
        Value::String(descriptor) => {
            let id = descriptor.strip_prefix("commit/").ok_or_else(|| {
                QueryError::internal(format!("Version Procedure returned invalid {field}"))
            })?;
            HashId::from_hex(id).map(Some).map_err(|_| {
                QueryError::internal(format!("Version Procedure returned invalid {field}"))
            })
        }
        _ => Err(QueryError::internal(format!(
            "Version Procedure returned non-Commit {field}"
        ))),
    }
}

fn validate_options(name: &str, options: &ExecutionOptions) -> QueryResult<()> {
    let historical_snapshot =
        !matches!(options.snapshot, SnapshotSelector::Current) && options.write_branch.is_none();
    if historical_snapshot {
        return Err(if super::registry::is_version_mutation(name) {
            QueryError::read_only_snapshot(
                "mutating Version Procedures cannot execute with options.at",
            )
        } else {
            QueryError::invalid_argument("Version Procedures do not accept options.at")
        });
    }
    let explicit_branch = options.write_branch.is_some();
    if explicit_branch
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "lithograph.commit.create"
                | "lithograph.patch.apply"
                | "lithograph.merge.start"
                | "lithograph.rebase"
                | "lithograph.squash"
                | "lithograph.reset"
                | "lithograph.revert"
        )
    {
        return Err(QueryError::invalid_argument(format!(
            "Version Procedure {name} does not accept options.branch"
        )));
    }
    let metadata = options.author_present || options.message_present;
    if metadata
        && !matches!(
            name.to_ascii_lowercase().as_str(),
            "lithograph.commit.create"
                | "lithograph.patch.apply"
                | "lithograph.merge.finalize"
                | "lithograph.squash"
                | "lithograph.revert"
        )
    {
        return Err(QueryError::invalid_argument(format!(
            "Version Procedure {name} does not accept author/message options"
        )));
    }
    Ok(())
}

fn branch_create(
    connection: &Connection,
    args: Vec<Value>,
    pinned_commit: HashId,
) -> QueryResult<Vec<ProcedureRow>> {
    let name = string_arg(&args, 0, "name")?;
    validate_ref_input(name)?;
    let from = match args.get(1) {
        Some(value) => resolve_descriptor(connection, value, "from")?,
        None => pinned_commit,
    };
    storage::create_branch_ref(connection, name, from).map_err(map_ref_write_error)?;
    Ok(vec![row([
        ("name", Value::String(name.to_owned())),
        ("commit", commit_value(from)),
    ])])
}

fn branch_checkout(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let name = string_arg(&args, 0, "name")?;
    validate_ref_input(name)?;
    let commit = storage::set_active_branch(connection, name).map_err(|error| match error {
        storage::StorageError::NotFound(_) => branch_not_found(name),
        error => error.into(),
    })?;
    Ok(vec![row([
        ("name", Value::String(name.to_owned())),
        ("commit", commit_value(commit)),
    ])])
}

fn branch_list(connection: &Connection) -> QueryResult<Vec<ProcedureRow>> {
    let active = storage::active_branch(connection)?;
    storage::list_branches(connection)?
        .into_iter()
        .map(|item| {
            Ok(row([
                ("name", Value::String(item.name.clone())),
                ("commit", commit_value(item.commit)),
                ("active", Value::Boolean(item.name == active)),
            ]))
        })
        .collect()
}

fn branch_delete(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let name = string_arg(&args, 0, "name")?;
    validate_ref_input(name)?;
    if name == "main" {
        return Err(QueryError::invalid_argument(
            "the main Branch cannot be deleted",
        ));
    }
    if storage::active_branch(connection)? == name {
        return Err(QueryError::invalid_argument(
            "the active Branch cannot be deleted",
        ));
    }
    let previous = storage::delete_branch_ref(connection, name)
        .map_err(QueryError::from)?
        .ok_or_else(|| branch_not_found(name))?;
    Ok(vec![row([
        ("name", Value::String(name.to_owned())),
        ("previousCommit", commit_value(previous)),
    ])])
}

fn tag_create(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let name = string_arg(&args, 0, "name")?;
    validate_ref_input(name)?;
    let target = resolve_descriptor_at(connection, &args, 1, "target")?;
    storage::create_tag(connection, name, target).map_err(map_ref_write_error)?;
    Ok(vec![row([
        ("name", Value::String(name.to_owned())),
        ("commit", commit_value(target)),
    ])])
}

fn tag_list(connection: &Connection) -> QueryResult<Vec<ProcedureRow>> {
    storage::list_tags(connection)?
        .into_iter()
        .map(|item| {
            Ok(row([
                ("name", Value::String(item.name)),
                ("commit", commit_value(item.commit)),
            ]))
        })
        .collect()
}

fn tag_move(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let name = string_arg(&args, 0, "name")?;
    validate_ref_input(name)?;
    let target = resolve_descriptor_at(connection, &args, 1, "target")?;
    let previous = existing_tag_result(storage::move_tag(connection, name, target), name)?;
    Ok(vec![row([
        ("name", Value::String(name.to_owned())),
        ("previousCommit", commit_value(previous)),
        ("commit", commit_value(target)),
    ])])
}

fn tag_delete(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let name = string_arg(&args, 0, "name")?;
    validate_ref_input(name)?;
    let previous = existing_tag_result(storage::delete_tag(connection, name), name)?;
    Ok(vec![row([
        ("name", Value::String(name.to_owned())),
        ("previousCommit", commit_value(previous)),
    ])])
}

fn existing_tag_result(
    result: Result<Option<HashId>, storage::StorageError>,
    name: &str,
) -> QueryResult<HashId> {
    result
        .map_err(QueryError::from)?
        .ok_or_else(|| tag_not_found(name))
}

fn commit_get(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let commit = resolve_descriptor_at(connection, &args, 0, "version")?;
    let record = storage::load_commit(connection, commit).map_err(map_version_error)?;
    let data = storage::commit_data(connection, commit)?;
    let (has_data, data_value) = match data {
        Some(text) => (
            true,
            json_to_value(serde_json::from_str(&text).map_err(|_| {
                QueryError::new(
                    QueryErrorKind::Storage,
                    "persisted Commit Data is invalid JSON",
                )
            })?)?,
        ),
        None => (false, Value::Null),
    };
    Ok(vec![row([
        ("commit", commit_value(commit)),
        ("parents", parents_value(record.parent1, record.parent2)),
        ("author", nullable_string(record.metadata.author)),
        ("message", nullable_string(record.metadata.message)),
        ("committedAt", Value::Integer(record.metadata.committed_at)),
        ("hasData", Value::Boolean(has_data)),
        ("data", data_value),
    ])])
}

fn commit_create(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_commit: HashId,
) -> QueryResult<Vec<ProcedureRow>> {
    let branch = target_branch(connection, options)?;
    ensure_target_branch_head(connection, &branch, pinned_commit)?;
    let metadata = CommitMetadata {
        author: options.author.clone(),
        message: options.message.clone(),
        committed_at: now_micros()?,
    };
    let commit = storage::create_empty_commit(connection, &branch, pinned_commit, &metadata)
        .map_err(|error| map_target_branch_error(error, &branch))?;
    if let Some(data) = args.first() {
        let data = value_to_json(data)?;
        storage::set_commit_data(
            connection,
            commit,
            &serde_json::to_string(&data)
                .map_err(|_| QueryError::internal("failed to serialize Commit Data"))?,
        )?;
    }
    Ok(vec![row([("commit", commit_value(commit))])])
}

fn commit_data_set(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let commit = resolve_descriptor_at(connection, &args, 0, "version")?;
    let data = args
        .get(1)
        .ok_or_else(|| QueryError::invalid_argument("missing data"))?;
    let json = value_to_json(data)?;
    let text = serde_json::to_string(&json)
        .map_err(|_| QueryError::internal("failed to serialize Commit Data"))?;
    storage::set_commit_data(connection, commit, &text)?;
    Ok(vec![row([
        ("commit", commit_value(commit)),
        ("data", data.clone()),
    ])])
}

fn commit_data_clear(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let commit = resolve_descriptor_at(connection, &args, 0, "version")?;
    storage::clear_commit_data(connection, commit)?;
    Ok(vec![row([("commit", commit_value(commit))])])
}

fn log(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let (mut traversal, limit) = log_request(connection, &args)?;
    collect_log_rows(connection, &mut traversal, limit)
}

fn log_request(connection: &Connection, args: &[Value]) -> QueryResult<(LogTraversal, usize)> {
    let cursor = args
        .get(2)
        .map(|value| string_value(value, "cursor"))
        .transpose()?;
    let traversal = if let Some(cursor) = cursor {
        decode_log_cursor(connection, cursor)?
    } else {
        let start = match args.first() {
            None => active_head(connection)?,
            Some(value) => resolve_descriptor(connection, value, "version")?,
        };
        new_log_traversal(connection, start)?
    };
    let limit = match args.get(1) {
        None => 100_usize,
        Some(Value::Integer(value)) if *value > 0 => usize::try_from(*value)
            .map_err(|_| QueryError::invalid_argument("log limit is too large"))?,
        Some(_) => {
            return Err(QueryError::invalid_argument(
                "log limit must be a positive Integer",
            ));
        }
    };
    Ok((traversal, limit))
}

fn collect_log_rows(
    connection: &Connection,
    traversal: &mut LogTraversal,
    limit: usize,
) -> QueryResult<Vec<ProcedureRow>> {
    let mut rows = Vec::with_capacity(limit.min(100));
    for _ in 0..limit {
        let Some(record) = next_log_commit(connection, traversal)? else {
            break;
        };
        let cursor = if traversal.ready.is_empty() && traversal.pending_children.is_empty() {
            Value::Null
        } else {
            Value::String(encode_log_cursor(traversal)?)
        };
        rows.push(row([
            ("commit", commit_value(record.id)),
            ("parents", parents_value(record.parent1, record.parent2)),
            ("author", nullable_string(record.metadata.author)),
            ("message", nullable_string(record.metadata.message)),
            ("committedAt", Value::Integer(record.metadata.committed_at)),
            ("cursor", cursor),
        ]));
    }
    Ok(rows)
}

fn new_log_traversal(connection: &Connection, start: HashId) -> QueryResult<LogTraversal> {
    let record = storage::load_commit(connection, start).map_err(map_version_error)?;
    Ok(LogTraversal {
        start,
        ready: BTreeSet::from([(Reverse(record.metadata.committed_at), start)]),
        pending_children: BTreeMap::new(),
    })
}

fn next_log_commit(
    connection: &Connection,
    traversal: &mut LogTraversal,
) -> QueryResult<Option<storage::CommitRecord>> {
    let Some((_, commit)) = traversal.ready.pop_first() else {
        if traversal.pending_children.is_empty() {
            return Ok(None);
        }
        return Err(QueryError::new(
            QueryErrorKind::Storage,
            "Commit DAG traversal has pending parents but no ready Commit",
        ));
    };
    let record = storage::load_commit(connection, commit).map_err(map_version_error)?;
    let parents = [record.parent1, record.parent2]
        .into_iter()
        .flatten()
        .collect::<BTreeSet<_>>();
    for parent in parents {
        if traversal
            .ready
            .iter()
            .any(|(_, ready_commit)| *ready_commit == parent)
        {
            return Err(QueryError::new(
                QueryErrorKind::Storage,
                "Commit DAG traversal observed a parent that was already ready",
            ));
        }
        if !traversal.pending_children.contains_key(&parent) {
            let count = storage::reachable_child_count(connection, traversal.start, parent)?;
            if count == 0 {
                return Err(QueryError::new(
                    QueryErrorKind::Storage,
                    "Commit DAG traversal found a parent with no reachable child",
                ));
            }
            traversal.pending_children.insert(parent, count);
        }
        let remaining = traversal
            .pending_children
            .get_mut(&parent)
            .ok_or_else(|| QueryError::internal("log traversal lost parent state"))?;
        *remaining = remaining.checked_sub(1).ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                "Commit DAG traversal child count underflowed",
            )
        })?;
        if *remaining == 0 {
            traversal.pending_children.remove(&parent);
            let parent_record =
                storage::load_commit(connection, parent).map_err(map_version_error)?;
            traversal
                .ready
                .insert((Reverse(parent_record.metadata.committed_at), parent));
        }
    }
    Ok(Some(record))
}

fn target_branch(connection: &Connection, options: &ExecutionOptions) -> QueryResult<String> {
    super::options::writable_branch(connection, options)
}

pub(crate) fn current_operation_commit(
    connection: &Connection,
    options: &ExecutionOptions,
) -> QueryResult<HashId> {
    let branch = target_branch(connection, options)?;
    storage::branch_head(connection, &branch)
        .map_err(|error| map_target_branch_error(error, &branch))
}

fn active_head(connection: &Connection) -> QueryResult<HashId> {
    let branch = storage::active_branch(connection)?;
    storage::branch_head(connection, &branch).map_err(|error| match error {
        storage::StorageError::NotFound(_) => branch_not_found(&branch),
        error => error.into(),
    })
}

fn resolve_descriptor_at(
    connection: &Connection,
    args: &[Value],
    index: usize,
    role: &str,
) -> QueryResult<HashId> {
    let value = args
        .get(index)
        .ok_or_else(|| QueryError::invalid_argument(format!("missing {role}")))?;
    resolve_descriptor(connection, value, role)
}

fn resolve_descriptor(connection: &Connection, value: &Value, role: &str) -> QueryResult<HashId> {
    let descriptor = string_value(value, role)?;
    let (kind, name) = descriptor.split_once('/').ok_or_else(|| {
        QueryError::invalid_argument(format!(
            "{role} must use branch/<name>, tag/<name>, or commit/<id>"
        ))
    })?;
    match kind {
        "branch" | "tag" => {
            storage::validate_ref_name(name).map_err(map_ref_input_error)?;
        }
        "commit"
            if name.len() == 64
                && !name.contains('/')
                && name.bytes().all(|byte| byte.is_ascii_hexdigit()) => {}
        "commit" => {
            return Err(QueryError::invalid_argument(format!(
                "{role} commit descriptor must contain a 64-character hexadecimal id"
            )));
        }
        _ => {
            return Err(QueryError::invalid_argument(format!(
                "{role} must use branch/<name>, tag/<name>, or commit/<id>"
            )));
        }
    }
    storage::resolve_version_descriptor(connection, descriptor).map_err(|error| {
        match (kind, error) {
            ("branch", storage::StorageError::NotFound(_)) => branch_not_found(name),
            ("tag", storage::StorageError::NotFound(_)) => tag_not_found(name),
            (_, storage::StorageError::NotFound(message)) => QueryError::new(
                QueryErrorKind::VersionNotFound,
                format!("Version was not found: {message}"),
            ),
            (_, error) => error.into(),
        }
    })
}

fn string_arg<'a>(args: &'a [Value], index: usize, role: &str) -> QueryResult<&'a str> {
    args.get(index)
        .ok_or_else(|| QueryError::invalid_argument(format!("missing {role}")))
        .and_then(|value| string_value(value, role))
}

fn string_value<'a>(value: &'a Value, role: &str) -> QueryResult<&'a str> {
    match value {
        Value::String(value) if !value.is_empty() => Ok(value),
        _ => Err(QueryError::invalid_argument(format!(
            "{role} must be a non-empty String"
        ))),
    }
}

fn map_ref_write_error(error: storage::StorageError) -> QueryError {
    match error {
        storage::StorageError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
            if code.extended_code & 0xff == rusqlite::ffi::SQLITE_CONSTRAINT =>
        {
            QueryError::invalid_argument("ref already exists or violates a uniqueness constraint")
        }
        error => error.into(),
    }
}

fn validate_ref_input(name: &str) -> QueryResult<()> {
    storage::validate_ref_name(name).map_err(map_ref_input_error)
}

fn map_ref_input_error(error: storage::StorageError) -> QueryError {
    match error {
        storage::StorageError::Corrupt(message) => QueryError::invalid_argument(message),
        error => error.into(),
    }
}

fn map_version_error(error: storage::StorageError) -> QueryError {
    match error {
        storage::StorageError::NotFound(message) => QueryError::new(
            QueryErrorKind::VersionNotFound,
            format!("Version was not found: {message}"),
        ),
        error => error.into(),
    }
}

fn ensure_target_branch_head(
    connection: &Connection,
    branch: &str,
    expected: HashId,
) -> QueryResult<()> {
    match storage::branch_head(connection, branch) {
        Ok(actual) if actual == expected => Ok(()),
        Ok(_) => Err(QueryError::new(
            QueryErrorKind::BranchHeadMoved,
            "branch head moved during Version operation",
        )),
        Err(error) => Err(map_target_branch_error(error, branch)),
    }
}

fn move_target_branch(
    connection: &Connection,
    branch: &str,
    expected: HashId,
    target: HashId,
) -> QueryResult<HashId> {
    match storage::move_branch_ref(connection, branch, Some(expected), target) {
        Ok(Some(previous)) => Ok(previous),
        Ok(None) => Err(branch_not_found(branch)),
        Err(error) => Err(map_target_branch_error(error, branch)),
    }
}

fn map_target_branch_error(error: storage::StorageError, branch: &str) -> QueryError {
    match error {
        storage::StorageError::NotFound(_) => branch_not_found(branch),
        error => error.into(),
    }
}

fn branch_not_found(name: &str) -> QueryError {
    QueryError::new(
        QueryErrorKind::BranchNotFound,
        format!("Branch branch/{name} was not found"),
    )
}

fn tag_not_found(name: &str) -> QueryError {
    QueryError::new(
        QueryErrorKind::TagNotFound,
        format!("Tag tag/{name} was not found"),
    )
}

fn commit_value(commit: HashId) -> Value {
    Value::String(format!("commit/{}", commit.to_hex()))
}

fn parents_value(parent1: Option<HashId>, parent2: Option<HashId>) -> Value {
    Value::List(
        [parent1, parent2]
            .into_iter()
            .flatten()
            .map(commit_value)
            .collect(),
    )
}

fn nullable_string(value: Option<String>) -> Value {
    value.map(Value::String).unwrap_or(Value::Null)
}

fn row<const N: usize>(values: [(&str, Value); N]) -> ProcedureRow {
    values
        .into_iter()
        .map(|(name, value)| (name.to_owned(), value))
        .collect()
}

pub(crate) fn value_to_json(value: &Value) -> QueryResult<serde_json::Value> {
    match value {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Boolean(value) => Ok((*value).into()),
        Value::Integer(value) => Ok((*value).into()),
        Value::Float(value) if value.is_finite() => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| QueryError::invalid_argument("JSON number is not finite")),
        Value::String(value) => Ok(value.clone().into()),
        Value::List(values) => values
            .iter()
            .map(value_to_json)
            .collect::<QueryResult<Vec<_>>>()
            .map(serde_json::Value::Array),
        Value::Map(values) => values
            .iter()
            .map(|(key, value)| Ok((key.clone(), value_to_json(value)?)))
            .collect::<QueryResult<serde_json::Map<_, _>>>()
            .map(serde_json::Value::Object),
        _ => Err(QueryError::invalid_argument(
            "Commit Data must be JSON-compatible Cypher data",
        )),
    }
}

pub(crate) fn json_to_value(value: serde_json::Value) -> QueryResult<Value> {
    match value {
        serde_json::Value::Null => Ok(Value::Null),
        serde_json::Value::Bool(value) => Ok(Value::Boolean(value)),
        serde_json::Value::Number(value) => {
            if let Some(integer) = value.as_i64() {
                Ok(Value::Integer(integer))
            } else if let Some(float) = value.as_f64() {
                Ok(Value::Float(float))
            } else {
                Err(QueryError::new(
                    QueryErrorKind::Storage,
                    "JSON number is unsupported",
                ))
            }
        }
        serde_json::Value::String(value) => Ok(Value::String(value)),
        serde_json::Value::Array(values) => values
            .into_iter()
            .map(json_to_value)
            .collect::<QueryResult<Vec<_>>>()
            .map(Value::List),
        serde_json::Value::Object(values) => values
            .into_iter()
            .map(|(key, value)| Ok((key, json_to_value(value)?)))
            .collect::<QueryResult<BTreeMap<_, _>>>()
            .map(Value::Map),
    }
}

fn encode_log_cursor(traversal: &LogTraversal) -> QueryResult<String> {
    let ready = traversal
        .ready
        .iter()
        .map(|(_, commit)| commit.to_hex())
        .collect::<Vec<_>>()
        .join(",");
    let pending = traversal
        .pending_children
        .iter()
        .map(|(commit, count)| format!("{},{count}", commit.to_hex()))
        .collect::<Vec<_>>()
        .join(";");
    let payload = format!("log2:{}:{ready}:{pending}", traversal.start.to_hex());
    let checksum = blake3::hash(payload.as_bytes()).to_hex();
    Ok(format!("{payload}:{}", &checksum.as_str()[..16]))
}

fn decode_log_cursor(connection: &Connection, cursor: &str) -> QueryResult<LogTraversal> {
    let parts = validated_log_cursor_parts(cursor)?;
    let start = HashId::from_hex(parts[1])
        .map_err(|_| QueryError::invalid_argument("invalid log cursor"))?;
    storage::load_commit(connection, start).map_err(map_version_error)?;
    let ready = decode_log_ready(connection, parts[2])?;
    let pending_children = decode_log_pending(connection, parts[3], &ready)?;
    if ready.is_empty() && !pending_children.is_empty() {
        return Err(QueryError::invalid_argument("invalid log cursor"));
    }
    Ok(LogTraversal {
        start,
        ready,
        pending_children,
    })
}

fn validated_log_cursor_parts(cursor: &str) -> QueryResult<Vec<&str>> {
    let parts = cursor.split(':').collect::<Vec<_>>();
    if parts.len() != 5 || parts[0] != "log2" {
        return Err(QueryError::invalid_argument("invalid log cursor"));
    }
    let payload = format!("{}:{}:{}:{}", parts[0], parts[1], parts[2], parts[3]);
    let checksum = blake3::hash(payload.as_bytes()).to_hex();
    if parts[4] != &checksum.as_str()[..16] {
        return Err(QueryError::invalid_argument("invalid log cursor"));
    }
    Ok(parts)
}

fn decode_log_ready(
    connection: &Connection,
    encoded_ready: &str,
) -> QueryResult<BTreeSet<(Reverse<i64>, HashId)>> {
    let mut ready = BTreeSet::new();
    for encoded in encoded_ready.split(',').filter(|value| !value.is_empty()) {
        let commit = HashId::from_hex(encoded)
            .map_err(|_| QueryError::invalid_argument("invalid log cursor"))?;
        let record = storage::load_commit(connection, commit).map_err(map_version_error)?;
        if !ready.insert((Reverse(record.metadata.committed_at), commit)) {
            return Err(QueryError::invalid_argument("invalid log cursor"));
        }
    }
    Ok(ready)
}

fn decode_log_pending(
    connection: &Connection,
    encoded_pending: &str,
    ready: &BTreeSet<(Reverse<i64>, HashId)>,
) -> QueryResult<BTreeMap<HashId, usize>> {
    let mut pending_children = BTreeMap::new();
    for encoded in encoded_pending.split(';').filter(|value| !value.is_empty()) {
        let fields = encoded.split(',').collect::<Vec<_>>();
        if fields.len() != 2 {
            return Err(QueryError::invalid_argument("invalid log cursor"));
        }
        let commit = HashId::from_hex(fields[0])
            .map_err(|_| QueryError::invalid_argument("invalid log cursor"))?;
        let count = fields[1]
            .parse::<usize>()
            .ok()
            .filter(|count| *count > 0)
            .ok_or_else(|| QueryError::invalid_argument("invalid log cursor"))?;
        if ready.iter().any(|(_, value)| *value == commit)
            || pending_children.insert(commit, count).is_some()
        {
            return Err(QueryError::invalid_argument("invalid log cursor"));
        }
        storage::load_commit(connection, commit).map_err(map_version_error)?;
    }
    Ok(pending_children)
}
