use chrono::Utc;
use rusqlite::Connection;

use crate::storage::{self, CommitMetadata, HashId, SchemaState, Snapshot};

use super::super::{QueryResult, check_interrupted};
use super::command::{PreparedSchema, SchemaCounters};
use super::validate::validate_schema_transition;

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
) -> QueryResult<SchemaOutcome> {
    check_interrupted(is_interrupted)?;
    let snapshot = Snapshot::resolve(connection, base_commit)?;
    let previous = SchemaState::load(connection, base_commit)?;
    validate_schema_transition(connection, &previous, &prepared.state, &snapshot)?;
    check_interrupted(is_interrupted)?;
    let schema_hash = prepared.state.persist(connection)?;
    let metadata = CommitMetadata {
        author: prepared.author.clone(),
        message: prepared.message.clone(),
        committed_at: Utc::now().timestamp_micros(),
    };
    let commit = storage::commit_schema(
        connection,
        &prepared.branch,
        base_commit,
        schema_hash,
        &metadata,
    )?;
    super::index::ensure_standard_indexes_for_commit(
        connection,
        commit,
        &previous,
        &prepared.state,
        is_interrupted,
    )?;
    Ok(SchemaOutcome {
        commit,
        counters: prepared.counters.clone(),
    })
}
