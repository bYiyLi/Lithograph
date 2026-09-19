use super::*;

pub(super) struct AdapterExecution {
    cursor: query::QueryCursor,
    columns: Vec<String>,
    read_guard: Option<MainReadGuard>,
}

impl AdapterExecution {
    pub(super) fn prepare(
        connection: &Connection,
        query_text: &str,
        params_text: &str,
        options_text: &str,
    ) -> LithographResult<Self> {
        let metadata = require_initialized(connection)?;
        let mut read_guard = Some(MainReadGuard::acquire(connection)?);
        let params = cypher::decode_parameters_text(params_text)
            .map_err(|error| LithographError::invalid_argument(error.message))?;
        let options = query::ExecutionOptions::parse_text(options_text).map_err(map_query_error)?;
        let prepared =
            query::prepare(connection, query_text, params, options).map_err(map_query_error)?;
        let semantic_maintenance = prepared.has_semantic_maintenance();
        let columns = prepared.columns.clone();
        let cursor = query::QueryCursor::new(prepared);
        if cursor.is_write() || semantic_maintenance {
            read_guard.take();
            require_no_active_readers(connection)?;
            require_current_storage_format(&metadata)?;
        }
        Ok(Self {
            cursor,
            columns,
            read_guard,
        })
    }

    pub(super) fn columns(&self) -> &[String] {
        &self.columns
    }

    pub(super) fn is_write(&self) -> bool {
        self.cursor.is_write()
    }

    pub(super) fn requires_transaction_boundary(&self) -> bool {
        self.cursor.requires_transaction_boundary()
    }

    pub(super) fn has_external_io(&self) -> bool {
        self.cursor.has_external_io()
    }

    pub(super) fn has_version_operation(&self) -> bool {
        self.cursor.has_version_operation()
    }

    pub(super) fn set_transaction_time_micros(&mut self, micros: i64) -> LithographResult<()> {
        self.cursor
            .set_transaction_time_micros(micros)
            .map_err(map_query_error)
    }

    pub(super) fn suppress_summary_commit(&mut self) {
        self.cursor.suppress_summary_commit();
    }

    pub(super) fn next_batch(
        &mut self,
        connection: &Connection,
        max_rows: usize,
    ) -> LithographResult<query::QueryBatch> {
        // SAFETY: AdapterExecution is invoked only with the live SQLite
        // connection that owns this query. Lithograph requires SQLite >= 3.45,
        // where sqlite3_is_interrupted() is part of the loadable-extension API.
        let db = unsafe { connection.handle() };
        let is_interrupted = || host_is_interrupted(db);
        self.cursor
            .next_batch_with_interrupt(connection, max_rows, &is_interrupted)
            .map_err(map_query_error)
    }

    pub(super) fn cancel(&mut self, connection: &Connection) -> LithographResult<()> {
        let result = self.cursor.cancel(connection).map_err(map_query_error);
        self.read_guard.take();
        result
    }

    pub(super) fn complete(
        &mut self,
        connection: &Connection,
    ) -> LithographResult<query::QuerySummary> {
        // SAFETY: AdapterExecution only uses the live SQLite connection that
        // owns this query, and SQLite >= 3.45 exposes interrupt state through
        // the loadable-extension API table.
        let db = unsafe { connection.handle() };
        let is_interrupted = || host_is_interrupted(db);
        let result = self
            .cursor
            .complete_with_interrupt(connection, &is_interrupted)
            .map_err(map_query_error);
        self.read_guard.take();
        result
    }
}

pub(super) fn scalar_result(
    connection: &Connection,
    query_text: &str,
    params_text: &str,
    options_text: &str,
) -> LithographResult<String> {
    let mut execution =
        AdapterExecution::prepare(connection, query_text, params_text, options_text)?;
    if execution.requires_transaction_boundary() {
        return Err(execution::map_query_error(
            query::QueryError::transaction_boundary_required(
                "SQL Bridge cannot execute CALL subqueries IN TRANSACTIONS; use the Native API",
            ),
        ));
    }
    if execution.is_write() {
        return with_savepoint(connection, |connection| {
            collect_scalar_result(connection, &mut execution)
        });
    }
    collect_scalar_result(connection, &mut execution)
}

fn collect_scalar_result(
    connection: &Connection,
    execution: &mut AdapterExecution,
) -> LithographResult<String> {
    let columns = execution.columns().to_vec();
    let mut rows = Vec::new();
    loop {
        let batch = execution.next_batch(connection, 256)?;
        rows.extend(batch.rows.iter().map(|row| row_json(row)));
        if batch.done {
            break;
        }
    }
    let summary = execution.complete(connection)?;
    let result = json!({
        "columns": columns,
        "rows": rows,
        "summary": summary_json(&summary),
    })
    .to_string();
    ensure_scalar_result_fits(connection, &result)?;
    Ok(result)
}

pub(super) fn ensure_scalar_result_fits(
    connection: &Connection,
    result: &str,
) -> LithographResult<()> {
    // SAFETY: this only reads the configured limit from the live connection;
    // passing -1 leaves the limit unchanged.
    let db = unsafe { connection.handle() };
    ensure_scalar_result_fits_handle(db, result)
}

pub(super) fn ensure_scalar_result_fits_handle(
    db: *mut ffi::sqlite3,
    result: &str,
) -> LithographResult<()> {
    // SAFETY: `db` is the live handle borrowed from `connection`, and -1 is
    // SQLite's documented read-only sentinel for this limit API.
    let limit = unsafe { ffi::sqlite3_limit(db, ffi::SQLITE_LIMIT_LENGTH, -1) };
    if limit >= 0 && result.len() > limit as usize {
        return Err(LithographError::new(
            ErrorCategory::Resource,
            "scalar result exceeds SQLite SQLITE_LIMIT_LENGTH",
            ffi::SQLITE_TOOBIG,
        ));
    }
    Ok(())
}

pub(super) fn row_json(row: &[cypher::Value]) -> Value {
    Value::Array(row.iter().map(cypher::encode_json).collect())
}

pub(super) fn summary_json(summary: &query::QuerySummary) -> Value {
    let mut value = json!({
        "queryType": summary.query_type.as_str(),
        "commit": summary.commit,
        "mergeSession": summary.merge_session.as_ref().map(|session| json!({
            "id": session.id,
            "revision": session.revision,
        })),
        "counters": counters_json(&summary.counters),
        "metrics": metrics_json(&summary.metrics),
    });
    if let Some(operators) = summary.metrics.operator_profile() {
        value["profile"] = json!({
            "operators": operators.iter().map(operator_profile_json).collect::<Vec<_>>(),
        });
    }
    value
}

fn operator_profile_json(operator: &query::OperatorRuntimeMetrics) -> Value {
    json!({
        "id": operator.id,
        "operator": operator.operator,
        "rows": operator.rows,
        "dbHits": operator.db_hits,
    })
}

fn metrics_json(metrics: &query::QueryMetrics) -> Value {
    json!({
        "rows": metrics.rows,
        "dbHits": metrics.db_hits,
        "elapsedMicros": metrics.elapsed_micros,
    })
}

pub(super) fn counters_json(counters: &query::QueryCounters) -> Value {
    json!({
        "nodesCreated": counters.nodes_created,
        "nodesDeleted": counters.nodes_deleted,
        "relationshipsCreated": counters.relationships_created,
        "relationshipsDeleted": counters.relationships_deleted,
        "propertiesSet": counters.properties_set,
        "propertiesRemoved": counters.properties_removed,
        "labelsAdded": counters.labels_added,
        "labelsRemoved": counters.labels_removed,
        "constraintsAdded": counters.constraints_added,
        "constraintsRemoved": counters.constraints_removed,
        "indexesAdded": counters.indexes_added,
        "indexesRemoved": counters.indexes_removed,
    })
}

pub(super) fn map_query_error(error: query::QueryError) -> LithographError {
    let category = match error.sqlite_code {
        Some(ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED) => ErrorCategory::Busy,
        Some(ffi::SQLITE_IOERR | ffi::SQLITE_CANTOPEN | ffi::SQLITE_READONLY) => ErrorCategory::Io,
        _ => match error.kind {
            query::QueryErrorKind::Parse => ErrorCategory::Parse,
            query::QueryErrorKind::Semantic => ErrorCategory::Semantic,
            query::QueryErrorKind::Type => ErrorCategory::Type,
            query::QueryErrorKind::Schema => ErrorCategory::Schema,
            query::QueryErrorKind::Constraint => ErrorCategory::Constraint,
            query::QueryErrorKind::InvalidArgument => ErrorCategory::InvalidArgument,
            query::QueryErrorKind::VersionNotFound => ErrorCategory::VersionNotFound,
            query::QueryErrorKind::BranchNotFound => ErrorCategory::BranchNotFound,
            query::QueryErrorKind::TagNotFound => ErrorCategory::TagNotFound,
            query::QueryErrorKind::GraphViewViolation => ErrorCategory::GraphViewViolation,
            query::QueryErrorKind::BranchHeadMoved => ErrorCategory::BranchHeadMoved,
            query::QueryErrorKind::MergeSessionNotFound => ErrorCategory::MergeSessionNotFound,
            query::QueryErrorKind::MergeSessionChanged => ErrorCategory::MergeSessionChanged,
            query::QueryErrorKind::MergeConflict => ErrorCategory::MergeConflict,
            query::QueryErrorKind::ReadOnlyAdapter => ErrorCategory::ReadOnlyAdapter,
            query::QueryErrorKind::ReadOnlySnapshot => ErrorCategory::ReadOnlySnapshot,
            query::QueryErrorKind::TransactionBoundaryRequired => {
                ErrorCategory::TransactionBoundaryRequired
            }
            query::QueryErrorKind::Busy => ErrorCategory::Busy,
            query::QueryErrorKind::Resource => ErrorCategory::Resource,
            query::QueryErrorKind::Io => ErrorCategory::Io,
            query::QueryErrorKind::Storage => ErrorCategory::Storage,
            query::QueryErrorKind::Interrupted => ErrorCategory::Resource,
            query::QueryErrorKind::Internal => ErrorCategory::Internal,
        },
    };
    let sqlite_code = error.sqlite_code.unwrap_or(match error.kind {
        query::QueryErrorKind::Interrupted => ffi::SQLITE_INTERRUPT,
        query::QueryErrorKind::Busy => ffi::SQLITE_BUSY,
        query::QueryErrorKind::Resource => ffi::SQLITE_TOOBIG,
        query::QueryErrorKind::Io => ffi::SQLITE_IOERR,
        _ => ffi::SQLITE_ERROR,
    });
    let message = match error.kind {
        query::QueryErrorKind::Storage => "query storage operation failed".to_owned(),
        query::QueryErrorKind::Busy => "query storage operation is busy".to_owned(),
        _ => error.message,
    };
    let mut mapped = LithographError::new(category, message, sqlite_code);
    mapped.line = error.line.map(u64::from);
    mapped.column = error.column.map(u64::from);
    mapped
}
