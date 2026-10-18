use chrono::Utc;
use rusqlite::Connection;

use crate::storage::{self, CommitMetadata, HashId, SchemaState, Snapshot};

use super::super::expression::clone_string_controlled;
use super::super::resources::{ExecutionControl, QueryResources, checked_add, checked_mul};
use super::super::version::VersionExecution;
use super::super::{QueryError, QueryResult};
use super::command::{PreparedSchema, SchemaCounters};
use super::validate::validate_schema_transition_controlled;

#[cfg(test)]
mod tests;

#[derive(Debug, Clone)]
pub(crate) struct SchemaOutcome {
    pub(crate) commit: HashId,
    pub(crate) counters: SchemaCounters,
}

pub(crate) fn execute_schema(
    connection: &Connection,
    prepared: &PreparedSchema,
    base_commit: HashId,
    is_interrupted: &dyn Fn() -> bool,
    resources: &QueryResources,
) -> QueryResult<SchemaOutcome> {
    let control = ExecutionControl::new(resources, is_interrupted);
    control.check()?;
    let execution = VersionExecution::new(connection, &control);
    let snapshot = execution.snapshot(base_commit)?;
    let previous = execution.load_schema(base_commit)?;
    let schema_hash =
        validate_and_persist_schema(connection, prepared, &previous, &snapshot, &control)?;
    let commit = commit_prepared_schema(connection, prepared, base_commit, schema_hash, &control)?;
    control.check()?;
    super::index::ensure_standard_indexes_for_commit_controlled(
        connection,
        commit,
        &previous,
        &prepared.state,
        &control,
    )?;
    Ok(SchemaOutcome {
        commit,
        counters: prepared.counters.clone(),
    })
}

fn validate_and_persist_schema(
    connection: &Connection,
    prepared: &PreparedSchema,
    previous: &SchemaState,
    snapshot: &Snapshot<'_>,
    control: &ExecutionControl<'_>,
) -> QueryResult<HashId> {
    // Metadata, canonical JSON DOM, ordered object buffers, final blob and the
    // storage verify copy may coexist. Reserve their conservative capacity
    // before validation or encoding, without walking graph entities in advance.
    let check = || control.check().map_err(QueryError::into_storage_error);
    let bytes = storage::schema_state_memory_bytes(&prepared.state, &check)?;
    let _workspace = control.reserve(checked_add(checked_mul(bytes, 16)?, 16_384)?)?;
    validate_schema_transition_controlled(
        connection,
        previous,
        &prepared.state,
        snapshot,
        control,
    )?;
    validate_search_transition(connection, previous, &prepared.state, control)?;
    let canonical = prepared.state.canonical_blob_checked(&check)?;
    control.check()?;
    let schema_hash = storage::persist_schema_blob(connection, &canonical)?;
    control.check()?;
    Ok(schema_hash)
}

fn commit_prepared_schema(
    connection: &Connection,
    prepared: &PreparedSchema,
    base_commit: HashId,
    schema_hash: HashId,
    control: &ExecutionControl<'_>,
) -> QueryResult<HashId> {
    let metadata_bytes = checked_add(
        prepared
            .author
            .as_ref()
            .map_or(0, |value| value.len() as u64),
        prepared
            .message
            .as_ref()
            .map_or(0, |value| value.len() as u64),
    )?;
    let _metadata = control.reserve(checked_add(checked_mul(metadata_bytes, 8)?, 4_096)?)?;
    let metadata = commit_metadata(prepared, control)?;
    let commit = storage::commit_schema(
        connection,
        &prepared.branch,
        base_commit,
        schema_hash,
        &metadata,
    )?;
    Ok(commit)
}

fn validate_search_transition(
    connection: &Connection,
    previous: &SchemaState,
    next: &SchemaState,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.check()?;
    let is_interrupted = || control.check().is_err();
    super::super::semantic_index::validate_fulltext_transition_controlled(
        connection,
        previous,
        next,
        &is_interrupted,
    )?;
    control.check()?;
    super::super::managed_semantic::validate_transition_controlled(
        connection,
        previous,
        next,
        &is_interrupted,
    )?;
    control.check()
}

fn commit_metadata(
    prepared: &PreparedSchema,
    control: &ExecutionControl<'_>,
) -> QueryResult<CommitMetadata> {
    Ok(CommitMetadata {
        author: prepared
            .author
            .as_deref()
            .map(|value| clone_string_controlled(value, Some(control)))
            .transpose()?,
        message: prepared
            .message
            .as_deref()
            .map(|value| clone_string_controlled(value, Some(control)))
            .transpose()?,
        committed_at: Utc::now().timestamp_micros(),
    })
}
