use super::*;

pub(super) struct AdapterExecution {
    cursor: query::QueryCursor,
    columns: Vec<String>,
    read_guard: Option<MainReadGuard>,
    side_effect_guard: Option<SideEffectGuard>,
    explicit_transaction: bool,
    mutating: bool,
    terminal: bool,
}

impl AdapterExecution {
    pub(super) fn prepare(
        connection: &Connection,
        query_text: &str,
        params_text: &str,
        options_text: &str,
    ) -> LithographResult<Self> {
        require_connection_healthy(connection)?;
        require_no_active_side_effect(connection)?;
        // SAFETY: the connection remains live for this synchronous prepare path.
        let db = unsafe { connection.handle() };
        let explicit_state = explicit_transaction_state(db);
        match prepare_adapter_execution(
            connection,
            query_text,
            params_text,
            options_text,
            explicit_state.as_ref(),
        ) {
            Ok(execution) => Ok(execution),
            Err(error) if explicit_state.is_some() => {
                transaction::fail_closed_sql_transaction(connection, error)
            }
            Err(error) => Err(error),
        }
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

    pub(super) fn is_explicit_transaction(&self) -> bool {
        self.explicit_transaction
    }

    pub(super) fn has_connection_state_operation(&self) -> bool {
        self.cursor.has_connection_state_operation()
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
        let result = self
            .cursor
            .next_batch_with_interrupt(connection, max_rows, &is_interrupted)
            .map_err(map_query_error);
        match result {
            Ok(batch) => Ok(batch),
            Err(error) if self.explicit_transaction => {
                self.terminal = true;
                self.read_guard.take();
                transaction::fail_closed_sql_transaction(connection, error)
            }
            Err(error) => Err(error),
        }
    }

    pub(super) fn cancel(&mut self, connection: &Connection) -> LithographResult<()> {
        if self.terminal {
            self.read_guard.take();
            return Ok(());
        }
        let result = self.cursor.cancel(connection).map_err(map_query_error);
        self.read_guard.take();
        self.side_effect_guard.take();
        self.terminal = true;
        if self.explicit_transaction {
            let abort = transaction::abort_active_sql_transaction(connection);
            return match (result, abort) {
                (Ok(()), Ok(())) => Ok(()),
                (Err(error), Ok(())) | (Ok(()), Err(error)) => Err(error),
                (Err(error), Err(cleanup)) => Err(LithographError::internal(format!(
                    "execution cleanup failed ({}); explicit transaction abort also failed ({})",
                    error.message, cleanup.message
                ))),
            };
        }
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
        match result {
            Ok(summary) => {
                if self.explicit_transaction
                    && self.mutating
                    && let Err(error) = transaction::mark_explicit_transaction_mutated(db)
                {
                    self.terminal = true;
                    return transaction::fail_closed_sql_transaction(connection, error);
                }
                self.side_effect_guard.take();
                self.terminal = true;
                Ok(summary)
            }
            Err(error) if self.explicit_transaction => {
                self.terminal = true;
                transaction::fail_closed_sql_transaction(connection, error)
            }
            Err(error) => Err(error),
        }
    }
}

fn prepare_adapter_execution(
    connection: &Connection,
    query_text: &str,
    params_text: &str,
    options_text: &str,
    explicit_state: Option<&ExplicitTransactionState>,
) -> LithographResult<AdapterExecution> {
    let metadata = require_initialized(connection)?;
    let (mut cursor, columns, semantic_maintenance) = prepare_cursor(
        connection,
        query_text,
        params_text,
        options_text,
        explicit_state,
    )?;
    configure_explicit_cursor(&mut cursor, semantic_maintenance, explicit_state)?;
    let mutating = cursor.is_write();
    let side_effecting = mutating
        || semantic_maintenance
        || cursor.requires_transaction_boundary()
        || cursor.has_version_operation();
    let (read_guard, side_effect_guard) =
        acquire_execution_guards(connection, &metadata, side_effecting)?;
    Ok(AdapterExecution {
        cursor,
        columns,
        read_guard,
        side_effect_guard,
        explicit_transaction: explicit_state.is_some(),
        mutating,
        terminal: false,
    })
}

fn prepare_cursor(
    connection: &Connection,
    query_text: &str,
    params_text: &str,
    options_text: &str,
    explicit_state: Option<&ExplicitTransactionState>,
) -> LithographResult<(query::QueryCursor, Vec<String>, bool)> {
    let params = cypher::decode_parameters_text(params_text)
        .map_err(|error| LithographError::invalid_argument(error.message))?;
    let effective_options = match explicit_state {
        Some(state) => transaction::execution_options(options_text, &state.branch)?,
        None => options_text.to_owned(),
    };
    let options =
        query::ExecutionOptions::parse_text(&effective_options).map_err(map_query_error)?;
    let prepared =
        query::prepare(connection, query_text, params, options).map_err(map_query_error)?;
    query::initialize_connection_state(connection).map_err(map_query_error)?;
    let semantic_maintenance = prepared.has_semantic_maintenance();
    let columns = prepared.columns.clone();
    Ok((
        query::QueryCursor::new(prepared),
        columns,
        semantic_maintenance,
    ))
}

fn configure_explicit_cursor(
    cursor: &mut query::QueryCursor,
    semantic_maintenance: bool,
    explicit_state: Option<&ExplicitTransactionState>,
) -> LithographResult<()> {
    let Some(state) = explicit_state else {
        return Ok(());
    };
    if cursor.requires_transaction_boundary()
        || cursor.has_version_operation()
        || semantic_maintenance
    {
        return Err(LithographError::new(
            ErrorCategory::TransactionBoundaryRequired,
            "active explicit transaction cannot execute transaction-owning, version/ref, or committed-target maintenance operations",
            ffi::SQLITE_ERROR,
        ));
    }
    cursor
        .set_transaction_time_micros(state.started_at_micros)
        .map_err(map_query_error)?;
    cursor.suppress_summary_commit();
    Ok(())
}

fn acquire_execution_guards(
    connection: &Connection,
    metadata: &Metadata,
    side_effecting: bool,
) -> LithographResult<(Option<MainReadGuard>, Option<SideEffectGuard>)> {
    if !side_effecting {
        return Ok((Some(MainReadGuard::acquire(connection)?), None));
    }
    require_no_active_readers(connection)?;
    let side_effect_guard = SideEffectGuard::acquire(connection)?;
    require_current_storage_format(metadata)?;
    Ok((None, Some(side_effect_guard)))
}

pub(super) fn scalar_result(
    connection: &Connection,
    query_text: &str,
    params_text: &str,
    options_text: &str,
) -> LithographResult<String> {
    let mut execution =
        AdapterExecution::prepare(connection, query_text, params_text, options_text)?;
    if execution.is_write()
        && !execution.requires_transaction_boundary()
        && !execution.is_explicit_transaction()
        && !execution.has_connection_state_operation()
    {
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
    let summary = loop {
        let batch = execution.next_batch(connection, 256)?;
        rows.extend(batch.rows.iter().map(|row| row_json(row)));
        if batch.done {
            break batch.summary.ok_or_else(|| {
                LithographError::internal("terminal execution batch is missing summary")
            })?;
        }
    };
    let result = json!({
        "columns": columns,
        "rows": rows,
        "summary": summary_json(&summary),
    })
    .to_string();
    if let Err(error) = ensure_scalar_result_fits(connection, &result) {
        return match execution.cancel(connection) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup),
        };
    }
    execution.complete(connection)?;
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
            query::QueryErrorKind::ReadOnlySnapshot => ErrorCategory::ReadOnlySnapshot,
            query::QueryErrorKind::TransactionBoundaryRequired => {
                ErrorCategory::TransactionBoundaryRequired
            }
            query::QueryErrorKind::Busy => ErrorCategory::Busy,
            query::QueryErrorKind::Resource => ErrorCategory::Resource,
            query::QueryErrorKind::Io => ErrorCategory::Io,
            query::QueryErrorKind::Storage => ErrorCategory::Storage,
            query::QueryErrorKind::Interrupted => ErrorCategory::Interrupted,
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
