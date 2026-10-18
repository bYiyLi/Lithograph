use super::*;

pub(super) struct AdapterExecution {
    cursor: query::QueryCursor,
    columns: Vec<String>,
    _metadata_memory: query::MemoryReservation,
    access: query::AccessPolicy,
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
        // tx_begin creates the TEMP cleanup target before BEGIN, so even an
        // interrupt during parameter decoding can abort the staged transaction.
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

    pub(super) fn resources(&self) -> &query::QueryResources {
        self.cursor.resources()
    }

    pub(super) fn budgeted_row_json(
        &self,
        row: &[cypher::Value],
        is_interrupted: &dyn Fn() -> bool,
    ) -> LithographResult<budget::BudgetedJson> {
        budget::row_json(row, &self.access, self.resources(), is_interrupted)
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
        let result = self.recover_interrupt(connection, result);
        match result {
            Ok(batch) => Ok(batch),
            Err(error) if self.explicit_transaction && !self.terminal => {
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
        let result = self.recover_cancel_interrupt(connection, result);
        self.read_guard.take();
        self.side_effect_guard.take();
        self.terminal = true;
        if self.explicit_transaction && require_connection_healthy(connection).is_ok() {
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
        let result = self.recover_interrupt(connection, result);
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
            Err(error) if self.explicit_transaction && !self.terminal => {
                self.terminal = true;
                transaction::fail_closed_sql_transaction(connection, error)
            }
            Err(error) => Err(error),
        }
    }

    fn recover_interrupt<T>(
        &mut self,
        connection: &Connection,
        result: LithographResult<T>,
    ) -> LithographResult<T> {
        match result {
            Err(error) if self.owns_interrupt_cleanup() => {
                match interrupt_cleanup::recover(connection) {
                    Ok(false) => {}
                    Ok(true) => self.finish_interrupt_cleanup(),
                    Err(cleanup) => {
                        self.finish_interrupt_cleanup();
                        return Err(cleanup);
                    }
                }
                Err(error)
            }
            result => result,
        }
    }

    fn owns_interrupt_cleanup(&self) -> bool {
        self.mutating || self.explicit_transaction || self.side_effect_guard.is_some()
    }

    fn finish_interrupt_cleanup(&mut self) {
        self.read_guard.take();
        self.side_effect_guard.take();
        self.terminal = true;
    }

    fn recover_cancel_interrupt(
        &self,
        connection: &Connection,
        result: LithographResult<()>,
    ) -> LithographResult<()> {
        if self.owns_interrupt_cleanup() && interrupt_cleanup::recover(connection)? {
            match result {
                Ok(()) => Ok(()),
                Err(error) if error.sqlite_code == ffi::SQLITE_INTERRUPT => Ok(()),
                Err(error) => Err(error),
            }
        } else {
            result
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
    let (mut cursor, columns, semantic_maintenance, access, metadata_memory) = prepare_cursor(
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
    if side_effecting || explicit_state.is_some() {
        interrupt_cleanup::prepare(connection)?;
    }
    Ok(AdapterExecution {
        cursor,
        columns,
        _metadata_memory: metadata_memory,
        access,
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
) -> LithographResult<(
    query::QueryCursor,
    Vec<String>,
    bool,
    query::AccessPolicy,
    query::MemoryReservation,
)> {
    let effective_options = match explicit_state {
        Some(state) => transaction::execution_options(options_text, &state.branch)?,
        None => options_text.to_owned(),
    };
    let options =
        query::ExecutionOptions::parse_text(&effective_options).map_err(map_query_error)?;
    let resources = query::QueryResources::new(options.memory_limit_bytes);
    // SAFETY: the connection remains live throughout prepare.
    let db = unsafe { connection.handle() };
    let (params, decode_reservation) =
        budget::decode_parameters(params_text, &resources, &|| host_is_interrupted(db))?;
    // Core adopts the decoded parameters into its own lease before cloning
    // any values. The temporary serde tree has already been destroyed.
    drop(decode_reservation);
    let prepared = query::prepare_with_resources_and_interrupt(
        connection,
        query_text,
        params,
        options,
        resources,
        &|| host_is_interrupted(db),
    )
    .map_err(map_query_error)?;
    query::initialize_connection_state(connection).map_err(map_query_error)?;
    let semantic_maintenance = prepared.has_semantic_maintenance();
    let columns_bytes = prepared
        .columns
        .iter()
        .try_fold(
            query::resources::allocation_bytes::<String>(prepared.columns.len())
                .map_err(map_query_error)?,
            |bytes, name| query::resources::checked_add(bytes, name.len() as u64),
        )
        .map_err(map_query_error)?;
    let metadata_memory = prepared
        .resources
        .reserve(columns_bytes)
        .map_err(map_query_error)?;
    let columns = prepared.columns.clone();
    let access = prepared.access_policy().clone();
    Ok((
        query::QueryCursor::new(prepared),
        columns,
        semantic_maintenance,
        access,
        metadata_memory,
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
    let result = collect_scalar_candidate(connection, execution);
    match result {
        Ok(result) => Ok(result),
        Err(error) => match execution.cancel(connection) {
            Ok(()) => Err(error),
            Err(cleanup) => Err(cleanup),
        },
    }
}

fn collect_scalar_candidate(
    connection: &Connection,
    execution: &mut AdapterExecution,
) -> LithographResult<String> {
    let resources = execution.resources().clone();
    // SAFETY: scalar collection borrows the live connection synchronously.
    let db = unsafe { connection.handle() };
    let mut retained = resources.reserve(0).map_err(map_query_error)?;
    let mut rows = Vec::new();
    let summary = collect_scalar_rows(connection, execution, &mut rows, &mut retained)?;
    let envelope = scalar_envelope(execution.columns(), &summary, rows, retained)?;
    let result = budget::serialize_json(&envelope.value, &resources, &|| host_is_interrupted(db))?;
    ensure_scalar_result_fits(connection, &result.text)?;
    execution.complete(connection)?;
    Ok(result.text)
}

fn collect_scalar_rows(
    connection: &Connection,
    execution: &mut AdapterExecution,
    rows: &mut Vec<Value>,
    retained: &mut query::MemoryReservation,
) -> LithographResult<query::QuerySummary> {
    // SAFETY: collection borrows this live connection synchronously.
    let db = unsafe { connection.handle() };
    loop {
        let batch = execution.next_batch(connection, 256)?;
        for row in &batch.rows {
            let encoded = execution.budgeted_row_json(row, &|| host_is_interrupted(db))?;
            append_scalar_row(rows, retained, encoded, execution.resources())?;
        }
        if batch.done {
            return batch.summary.ok_or_else(|| {
                LithographError::internal("terminal execution batch is missing summary")
            });
        }
    }
}

fn append_scalar_row(
    rows: &mut Vec<Value>,
    retained: &mut query::MemoryReservation,
    encoded: budget::BudgetedJson,
    resources: &query::QueryResources,
) -> LithographResult<()> {
    if rows.len() == rows.capacity() {
        let previous = query::resources::allocation_bytes::<Value>(rows.capacity())
            .map_err(map_query_error)?;
        let _growth_memory = resources.reserve(previous).map_err(map_query_error)?;
        let capacity = rows.capacity().max(4);
        let bytes =
            query::resources::allocation_bytes::<Value>(capacity).map_err(map_query_error)?;
        retained.grow(bytes).map_err(map_query_error)?;
        rows.reserve_exact(capacity);
    }
    retained
        .merge(encoded.reservation)
        .map_err(map_query_error)?;
    rows.push(encoded.value);
    Ok(())
}

fn scalar_envelope(
    columns: &[String],
    summary: &query::QuerySummary,
    rows: Vec<Value>,
    mut retained: query::MemoryReservation,
) -> LithographResult<budget::BudgetedJson> {
    // Rows move into the envelope; no second full row collection is made.
    // Keep the retained-row lease alive while the serialized String exists.
    let metadata_bytes = adapter_metadata_bytes(columns, summary)?;
    retained.grow(metadata_bytes).map_err(map_query_error)?;
    let mut object = serde_json::Map::new();
    object.insert("columns".to_owned(), json!(columns));
    object.insert("rows".to_owned(), Value::Array(rows));
    object.insert("summary".to_owned(), summary_json(summary));
    let value = Value::Object(object);
    retained
        .resize(query::resources::json_bytes(&value).map_err(map_query_error)?)
        .map_err(map_query_error)?;
    Ok(budget::BudgetedJson {
        value,
        reservation: retained,
    })
}

pub(super) fn adapter_metadata_bytes(
    columns: &[String],
    summary: &query::QuerySummary,
) -> LithographResult<u64> {
    let columns_bytes = columns
        .iter()
        .try_fold(0_u64, |bytes, name| {
            query::resources::checked_add(bytes, name.len() as u64 + 256)
        })
        .map_err(map_query_error)?;
    let profile_bytes = summary
        .metrics
        .operator_profile()
        .map_or(Ok(0), |operators| {
            operators.iter().try_fold(0_u64, |bytes, operator| {
                query::resources::checked_add(operator.operator.len() as u64, 8192)
                    .and_then(|additional| query::resources::checked_add(bytes, additional))
            })
        })
        .map_err(map_query_error)?;
    query::resources::checked_add(columns_bytes, profile_bytes)
        .and_then(|bytes| query::resources::checked_add(bytes, 65536))
        .map_err(map_query_error)
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

#[cfg(test)]
pub(super) fn row_json(row: &[cypher::Value], access: &query::AccessPolicy) -> Value {
    budget::row_json(row, access, &query::QueryResources::default(), &|| false)
        .expect("unlimited test encoding")
        .value
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
            query::QueryErrorKind::AccessDenied => ErrorCategory::AccessDenied,
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
        query::QueryErrorKind::AccessDenied => ffi::SQLITE_AUTH,
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
