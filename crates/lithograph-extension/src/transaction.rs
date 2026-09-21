use super::*;

#[derive(Debug)]
struct TxBeginOptions {
    branch: Option<String>,
    expected_head: Option<storage::HashId>,
    author: Option<String>,
    message: Option<String>,
}

pub(super) fn sql_tx_begin(
    connection: &Connection,
    options_json: &str,
    validate_result: impl FnOnce(&str) -> LithographResult<()>,
) -> LithographResult<String> {
    require_connection_healthy(connection)?;
    // SAFETY: the live rusqlite wrapper only lends its handle for this call.
    let db = unsafe { connection.handle() };
    if explicit_transaction_state(db).is_some() {
        return Err(transaction_boundary_error(
            "an explicit Lithograph transaction is already active on this connection",
        ));
    }
    let options = parse_tx_begin_options(options_json)?;
    let metadata = require_initialized(connection)?;
    require_current_storage_format(&metadata)?;
    require_no_active_side_effect(connection)?;
    require_no_active_readers(connection)?;
    if !connection.is_autocommit() {
        return Err(transaction_boundary_error(
            "Lithograph explicit transaction requires SQLite autocommit mode",
        ));
    }
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| map_sqlite_error(error, "failed to begin explicit transaction"))?;

    let begin = catch_unwind(AssertUnwindSafe(|| {
        begin_explicit_transaction_state(db, connection, options)
    }));
    let result = match begin {
        Ok(Ok(result)) => result,
        Ok(Err(error)) => return fail_closed_tx_with_connection(db, connection, error),
        Err(_) => {
            return fail_closed_tx_with_connection(
                db,
                connection,
                LithographError::internal("panic while starting explicit transaction"),
            );
        }
    };
    if let Err(error) = validate_result(&result) {
        return fail_closed_tx_with_connection(db, connection, error);
    }
    Ok(result)
}

fn begin_explicit_transaction_state(
    db: *mut ffi::sqlite3,
    connection: &Connection,
    options: TxBeginOptions,
) -> LithographResult<String> {
    let branch = match options.branch {
        Some(branch) => branch,
        None => storage::active_branch(connection)
            .map_err(|error| execution::map_query_error(error.into()))?,
    };
    let base_commit = explicit_branch_head(connection, &branch)?;
    if options
        .expected_head
        .is_some_and(|expected| expected != base_commit)
    {
        return Err(LithographError::new(
            ErrorCategory::BranchHeadMoved,
            "target Branch head does not match expectedHead",
            ffi::SQLITE_ERROR,
        ));
    }
    let state = ExplicitTransactionState {
        branch,
        base_commit,
        author: options.author,
        message: options.message,
        started_at_micros: transaction_now_micros()?,
        mutated: false,
    };
    store_explicit_transaction_state(db, state)?;
    Ok(json!({"baseCommit": format!("commit/{}", base_commit.to_hex())}).to_string())
}

fn explicit_branch_head(
    connection: &Connection,
    branch: &str,
) -> LithographResult<storage::HashId> {
    storage::branch_head(connection, branch).map_err(|error| {
        execution::map_query_error(match error {
            storage::StorageError::NotFound(_) => query::QueryError::new(
                query::QueryErrorKind::BranchNotFound,
                format!("Branch branch/{branch} was not found"),
            ),
            error => error.into(),
        })
    })
}

pub(super) fn mark_explicit_transaction_mutated(db: *mut ffi::sqlite3) -> LithographResult<()> {
    update_explicit_transaction_state(db, |state| {
        state.mutated = true;
        Ok(())
    })
}

pub(super) fn sql_tx_commit(connection: &Connection) -> LithographResult<String> {
    require_connection_healthy(connection)?;
    require_no_active_side_effect(connection)?;
    require_no_active_readers(connection)?;
    // SAFETY: the connection remains live throughout this synchronous call.
    let db = unsafe { connection.handle() };
    let operation = || {
        let state = explicit_transaction_state(db)
            .ok_or_else(|| transaction_misuse("no active explicit Lithograph transaction"))?;
        let result = finalize_explicit_transaction(connection, &state);
        let result = match result {
            Ok(result) => result,
            Err(error) => return fail_closed_tx_with_connection(db, connection, error),
        };
        if let Err(error) = execution::ensure_scalar_result_fits(connection, &result) {
            return fail_closed_tx_with_connection(db, connection, error);
        }
        if let Err(error) = connection.execute_batch("COMMIT") {
            return fail_closed_tx_with_connection(
                db,
                connection,
                map_sqlite_error(error, "failed to commit explicit transaction"),
            );
        }
        clear_explicit_transaction_state(db);
        Ok(result)
    };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => fail_closed_tx_with_connection(
            db,
            connection,
            LithographError::internal("panic while committing explicit transaction"),
        ),
    }
}

pub(super) fn sql_tx_abort(connection: &Connection) -> LithographResult<()> {
    require_connection_healthy(connection)?;
    require_no_active_side_effect(connection)?;
    require_no_active_readers(connection)?;
    // SAFETY: the connection remains live throughout this synchronous call.
    let db = unsafe { connection.handle() };
    if explicit_transaction_state(db).is_none() {
        return Err(transaction_misuse(
            "no active explicit Lithograph transaction",
        ));
    }
    match catch_unwind(AssertUnwindSafe(|| {
        rollback_explicit_transaction(db, connection)
    })) {
        Ok(result) => result,
        Err(_) => {
            quarantine_connection(connection);
            Err(LithographError::internal(
                "panic while aborting explicit transaction",
            ))
        }
    }
}

pub(super) fn fail_closed_sql_transaction<T>(
    connection: &Connection,
    error: LithographError,
) -> LithographResult<T> {
    // SAFETY: the connection remains live throughout this synchronous call.
    let db = unsafe { connection.handle() };
    if explicit_transaction_state(db).is_none() {
        return Err(error);
    }
    fail_closed_tx_with_connection(db, connection, error)
}

pub(super) fn abort_active_sql_transaction(connection: &Connection) -> LithographResult<()> {
    // SAFETY: the connection remains live throughout this synchronous call.
    let db = unsafe { connection.handle() };
    if explicit_transaction_state(db).is_none() {
        return Ok(());
    }
    rollback_explicit_transaction(db, connection)
}

pub(super) fn execution_options(text: &str, branch: &str) -> LithographResult<String> {
    let value: Value = serde_json::from_str(if text.is_empty() { "{}" } else { text })
        .map_err(|_| LithographError::invalid_argument("options must contain valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| LithographError::invalid_argument("options must be a JSON object"))?;
    for key in object.keys() {
        if key != "graphView" {
            return Err(LithographError::invalid_argument(format!(
                "explicit transaction execution does not accept option {key}"
            )));
        }
    }
    let mut options = serde_json::Map::new();
    options.insert("branch".to_owned(), Value::String(branch.to_owned()));
    if let Some(graph_view) = object.get("graphView") {
        options.insert("graphView".to_owned(), graph_view.clone());
    }
    Ok(Value::Object(options).to_string())
}

fn finalize_explicit_transaction(
    connection: &Connection,
    state: &ExplicitTransactionState,
) -> LithographResult<String> {
    if !state.mutated {
        let head = storage::branch_head(connection, &state.branch)
            .map_err(|error| execution::map_query_error(error.into()))?;
        if head != state.base_commit {
            return Err(LithographError::new(
                ErrorCategory::BranchHeadMoved,
                "target Branch changed during read-only explicit transaction",
                ffi::SQLITE_ERROR,
            ));
        }
        return Ok(json!({
            "commit": format!("commit/{}", state.base_commit.to_hex()),
            "counters": execution::counters_json(&query::QueryCounters::default()),
        })
        .to_string());
    }

    let staged_head = storage::branch_head(connection, &state.branch)
        .map_err(|error| execution::map_query_error(error.into()))?;
    let layer = storage::layer_between_commits(connection, state.base_commit, staged_head)
        .map_err(|error| execution::map_query_error(error.into()))?;
    let base_schema = storage::SchemaState::load(connection, state.base_commit)
        .map_err(|error| execution::map_query_error(error.into()))?;
    let final_schema = storage::SchemaState::load(connection, staged_head)
        .map_err(|error| execution::map_query_error(error.into()))?;
    let counters = transaction_counters(&layer, &base_schema, &final_schema);

    storage::move_branch_ref(
        connection,
        &state.branch,
        Some(staged_head),
        state.base_commit,
    )
    .map_err(|error| execution::map_query_error(error.into()))?;
    storage::discard_uncommitted_chain(connection, state.base_commit, staged_head)
        .map_err(|error| execution::map_query_error(error.into()))?;
    let schema_hash = final_schema
        .persist(connection)
        .map_err(|error| execution::map_query_error(error.into()))?;
    let metadata = storage::CommitMetadata {
        author: state.author.clone(),
        message: state.message.clone(),
        committed_at: transaction_now_micros()?,
    };
    let commit = storage::commit_layer_with_schema(
        connection,
        &state.branch,
        state.base_commit,
        None,
        &layer,
        schema_hash,
        &metadata,
    )
    .map_err(|error| execution::map_query_error(error.into()))?;
    Ok(json!({
        "commit": format!("commit/{}", commit.to_hex()),
        "counters": execution::counters_json(&counters),
    })
    .to_string())
}

fn transaction_counters(
    layer: &storage::LayerBuilder,
    before_schema: &storage::SchemaState,
    after_schema: &storage::SchemaState,
) -> query::QueryCounters {
    let layer_counts = layer.delta_counts();
    let mut counters = query::QueryCounters {
        nodes_created: layer_counts.nodes_created,
        nodes_deleted: layer_counts.nodes_deleted,
        relationships_created: layer_counts.relationships_created,
        relationships_deleted: layer_counts.relationships_deleted,
        properties_set: layer_counts.properties_set,
        properties_removed: layer_counts.properties_removed,
        labels_added: layer_counts.labels_added,
        labels_removed: layer_counts.labels_removed,
        ..query::QueryCounters::default()
    };
    let before_constraints = before_schema
        .constraints
        .keys()
        .collect::<std::collections::BTreeSet<_>>();
    let after_constraints = after_schema
        .constraints
        .keys()
        .collect::<std::collections::BTreeSet<_>>();
    counters.constraints_added = after_constraints.difference(&before_constraints).count() as u64;
    counters.constraints_removed = before_constraints.difference(&after_constraints).count() as u64;
    let before_indexes = before_schema
        .indexes
        .keys()
        .collect::<std::collections::BTreeSet<_>>();
    let after_indexes = after_schema
        .indexes
        .keys()
        .collect::<std::collections::BTreeSet<_>>();
    counters.indexes_added = after_indexes.difference(&before_indexes).count() as u64;
    counters.indexes_removed = before_indexes.difference(&after_indexes).count() as u64;
    counters
}

fn parse_tx_begin_options(text: &str) -> LithographResult<TxBeginOptions> {
    let value: Value =
        serde_json::from_str(if text.is_empty() { "{}" } else { text }).map_err(|_| {
            LithographError::invalid_argument("transaction options must contain valid JSON")
        })?;
    let object = value.as_object().ok_or_else(|| {
        LithographError::invalid_argument("transaction options must be a JSON object")
    })?;
    validate_tx_begin_option_keys(object)?;
    let branch = parse_tx_branch(object)?;
    let expected_head = parse_tx_expected_head(object)?;
    Ok(TxBeginOptions {
        branch,
        expected_head,
        author: optional_tx_string(object, "author", true)?,
        message: optional_tx_string(object, "message", true)?,
    })
}

fn parse_tx_expected_head(
    object: &serde_json::Map<String, Value>,
) -> LithographResult<Option<storage::HashId>> {
    optional_tx_string(object, "expectedHead", false)?
        .map(|descriptor| {
            let id = descriptor.strip_prefix("commit/").ok_or_else(|| {
                LithographError::invalid_argument("expectedHead must use commit/<id>")
            })?;
            storage::HashId::from_hex(id).map_err(|_| {
                LithographError::invalid_argument("expectedHead must use commit/<64-hex-id>")
            })
        })
        .transpose()
}

fn validate_tx_begin_option_keys(object: &serde_json::Map<String, Value>) -> LithographResult<()> {
    for key in object.keys() {
        if !matches!(
            key.as_str(),
            "branch" | "expectedHead" | "author" | "message"
        ) {
            return Err(LithographError::invalid_argument(format!(
                "unknown transaction option {key}"
            )));
        }
    }
    Ok(())
}

fn parse_tx_branch(object: &serde_json::Map<String, Value>) -> LithographResult<Option<String>> {
    let branch = optional_tx_string(object, "branch", false)?;
    if let Some(branch) = branch.as_deref() {
        storage::validate_ref_name(branch)
            .map_err(|error| LithographError::invalid_argument(error.to_string()))?;
    }
    Ok(branch)
}

fn optional_tx_string(
    object: &serde_json::Map<String, Value>,
    key: &str,
    nullable: bool,
) -> LithographResult<Option<String>> {
    match object.get(key) {
        None => Ok(None),
        Some(Value::Null) if nullable => Ok(None),
        Some(Value::String(value)) if !value.is_empty() || nullable => Ok(Some(value.clone())),
        _ => Err(LithographError::invalid_argument(format!(
            "transaction option {key} must be {}",
            if nullable {
                "a String or null"
            } else {
                "a non-empty String"
            }
        ))),
    }
}

fn transaction_now_micros() -> LithographResult<i64> {
    let elapsed = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| LithographError::internal("system clock is before Unix epoch"))?;
    i64::try_from(elapsed.as_micros())
        .map_err(|_| LithographError::internal("system clock exceeds supported timestamp range"))
}

fn transaction_boundary_error(message: impl Into<String>) -> LithographError {
    LithographError::new(
        ErrorCategory::TransactionBoundaryRequired,
        message,
        ffi::SQLITE_ERROR,
    )
}

fn transaction_misuse(message: impl Into<String>) -> LithographError {
    LithographError::new(ErrorCategory::InvalidArgument, message, ffi::SQLITE_MISUSE)
}

fn fail_closed_tx_with_connection<T>(
    db: *mut ffi::sqlite3,
    connection: &Connection,
    error: LithographError,
) -> LithographResult<T> {
    match rollback_explicit_transaction(db, connection) {
        Ok(()) => Err(error),
        Err(cleanup) => Err(LithographError::internal(format!(
            "explicit transaction cleanup failed after {}: {}",
            error.message, cleanup.message
        ))),
    }
}

fn rollback_explicit_transaction(
    db: *mut ffi::sqlite3,
    connection: &Connection,
) -> LithographResult<()> {
    let rollback = connection
        .execute_batch("ROLLBACK")
        .map_err(|error| map_sqlite_error(error, "failed to rollback explicit transaction"));
    clear_explicit_transaction_state(db);
    if rollback.is_err() {
        quarantine_connection(connection);
    }
    rollback
}
