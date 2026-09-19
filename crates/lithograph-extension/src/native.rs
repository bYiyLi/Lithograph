use super::*;

#[repr(C)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LithographEventKindV1 {
    Columns = 1,
    Row = 2,
    Summary = 3,
}

pub type LithographEventCallbackV1 = Option<
    unsafe extern "C" fn(
        user_data: *mut c_void,
        kind: LithographEventKindV1,
        json: *const u8,
        json_len: usize,
    ) -> c_int,
>;

struct NativeDbMutexGuard {
    mutex: *mut ffi::sqlite3_mutex,
}

impl NativeDbMutexGuard {
    fn for_registered(db: *mut ffi::sqlite3) -> Self {
        if db.is_null() || !native_connection_registered(db) {
            return Self {
                mutex: ptr::null_mut(),
            };
        }
        // SAFETY: registration proves the loadable-extension API table is
        // initialized and `db` is the caller-owned live connection handle.
        let mutex = unsafe { ffi::sqlite3_db_mutex(db) };
        if !mutex.is_null() {
            // SAFETY: SQLite owns this connection mutex. Serialized mode uses
            // a recursive mutex; non-serialized modes may return NULL.
            unsafe { ffi::sqlite3_mutex_enter(mutex) };
        }
        Self { mutex }
    }
}

impl Drop for NativeDbMutexGuard {
    fn drop(&mut self) {
        if !self.mutex.is_null() {
            // SAFETY: this guard entered the same SQLite-owned mutex and drops
            // before the caller-owned connection leaves the ABI invocation.
            unsafe { ffi::sqlite3_mutex_leave(self.mutex) };
        }
    }
}

/// Executes one Cypher query through the stable native ABI v1.
///
/// # Safety
///
/// `db` must be an existing SQLite connection on which this shared library has
/// already been loaded. Input pointer/length pairs must be readable for this
/// call. `error_json` memory must be released with [`lithograph_v1_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lithograph_v1_execute(
    db: *mut ffi::sqlite3,
    query: *const c_char,
    query_len: usize,
    params_json: *const c_char,
    params_len: usize,
    options_json: *const c_char,
    options_len: usize,
    callback: LithographEventCallbackV1,
    user_data: *mut c_void,
    error_json: *mut *mut c_char,
) -> c_int {
    let operation = || {
        // SAFETY: pointer/length validity is guaranteed by the public ABI
        // contract and inputs are copied before this invocation returns.
        unsafe {
            native_execute_impl(
                db,
                NativeTextInput::new(query, query_len),
                NativeTextInput::new(params_json, params_len),
                NativeTextInput::new(options_json, options_len),
                callback,
                user_data,
            )
        }
    };
    // SAFETY: the public ABI contract guarantees all non-NULL pointers are
    // valid for this call, including writable `error_json` out-parameter storage.
    unsafe { native_boundary(db, error_json, operation) }
}

unsafe fn native_boundary(
    db: *mut ffi::sqlite3,
    error_json: *mut *mut c_char,
    operation: impl FnOnce() -> LithographResult<()>,
) -> c_int {
    let _guard = NativeDbMutexGuard::for_registered(db);
    // SAFETY: callers guarantee a non-NULL pointer references writable ABI
    // out-parameter storage.
    unsafe { clear_error_out(error_json) };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => ffi::SQLITE_OK,
        Ok(Err(error)) => {
            // SAFETY: the same caller contract covers the error out-pointer.
            // SAFETY: the public ABI allows a writable error out-pointer or NULL.
            unsafe { write_native_error(error_json, &error) }
        }
        Err(_) => {
            // SAFETY: the same caller contract covers the error out-pointer.
            unsafe {
                write_native_error(
                    error_json,
                    &LithographError::internal("panic while executing native ABI call"),
                )
            }
        }
    }
}

#[derive(Clone, Copy)]
struct NativeTextInput {
    pointer: *const c_char,
    length: usize,
}

impl NativeTextInput {
    const fn new(pointer: *const c_char, length: usize) -> Self {
        Self { pointer, length }
    }
}

unsafe fn native_execute_impl(
    db: *mut ffi::sqlite3,
    query: NativeTextInput,
    params_json: NativeTextInput,
    options_json: NativeTextInput,
    callback: LithographEventCallbackV1,
    user_data: *mut c_void,
) -> LithographResult<()> {
    validate_native_execute_args(db, callback)?;
    require_native_connection_registered(db)?;
    if explicit_transaction_state(db).is_some() {
        return Err(transaction_boundary_error(
            "an explicit Lithograph transaction is active on this connection",
        ));
    }
    // SAFETY: all three input buffers remain readable for this ABI invocation.
    let inputs = unsafe { decode_native_execute_inputs(query, params_json, options_json)? };
    if inputs.query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty"));
    }
    // SAFETY: registration proves `db` is a live SQLite connection containing
    // this extension; rusqlite borrows the handle without taking ownership.
    let connection = unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "invalid SQLite connection"))?;
    require_no_explicit_transaction(&connection)?;
    let mut execution = execution::AdapterExecution::prepare(
        &connection,
        &inputs.query,
        &inputs.params_json,
        &inputs.options_json,
    )?;
    if execution.is_write() && !execution.requires_transaction_boundary() {
        return with_savepoint(&connection, |connection| {
            // SAFETY: callback and user_data originate from the active ABI invocation.
            unsafe { emit_execution_events(connection, &mut execution, callback, user_data) }
        });
    }
    // SAFETY: callback and user_data originate from the active ABI invocation.
    unsafe { emit_execution_events(&connection, &mut execution, callback, user_data) }
}

fn validate_native_execute_args(
    db: *mut ffi::sqlite3,
    callback: LithographEventCallbackV1,
) -> LithographResult<()> {
    require_native_db(db)?;
    if callback.is_none() {
        return Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            "callback must not be NULL",
            ffi::SQLITE_MISUSE,
        ));
    }
    Ok(())
}

struct NativeExecuteInputs {
    query: String,
    params_json: String,
    options_json: String,
}

unsafe fn decode_native_execute_inputs(
    query: NativeTextInput,
    params_json: NativeTextInput,
    options_json: NativeTextInput,
) -> LithographResult<NativeExecuteInputs> {
    // SAFETY: each pointer/length pair comes directly from the public ABI call
    // and remains readable for this invocation.
    let query = unsafe { input_utf8(query.pointer, query.length, "query")? };
    // SAFETY: same ABI lifetime contract as `query` above.
    let params = unsafe { input_utf8(params_json.pointer, params_json.length, "params_json")? };
    // SAFETY: same ABI lifetime contract as `query` above.
    let options = unsafe { input_utf8(options_json.pointer, options_json.length, "options_json")? };
    Ok(NativeExecuteInputs {
        query,
        params_json: params,
        options_json: options,
    })
}

unsafe fn emit_execution_events(
    connection: &Connection,
    execution: &mut execution::AdapterExecution,
    callback: LithographEventCallbackV1,
    user_data: *mut c_void,
) -> LithographResult<()> {
    let columns = serde_json::to_string(execution.columns()).map_err(|error| {
        LithographError::internal(format!("failed to encode result columns: {error}"))
    })?;
    // SAFETY: the callback and user-data pointer come from this ABI invocation.
    if let Err(error) = unsafe {
        emit_event(
            callback,
            user_data,
            LithographEventKindV1::Columns,
            &columns,
        )
    } {
        execution.cancel(connection)?;
        return Err(error);
    }

    loop {
        let batch = execution.next_batch(connection, 256)?;
        for row in &batch.rows {
            let payload = execution::row_json(row).to_string();
            // SAFETY: the callback and user-data pointer come from this ABI invocation.
            let event =
                unsafe { emit_event(callback, user_data, LithographEventKindV1::Row, &payload) };
            if let Err(error) = event {
                execution.cancel(connection)?;
                return Err(error);
            }
        }
        if batch.done {
            let summary = execution.complete(connection)?;
            let payload = execution::summary_json(&summary).to_string();
            // SAFETY: the callback and user-data pointer come from this ABI invocation.
            return unsafe {
                emit_event(
                    callback,
                    user_data,
                    LithographEventKindV1::Summary,
                    &payload,
                )
            };
        }
    }
}

unsafe fn emit_event(
    callback: LithographEventCallbackV1,
    user_data: *mut c_void,
    kind: LithographEventKindV1,
    payload: &str,
) -> LithographResult<()> {
    let callback = callback.ok_or_else(|| {
        LithographError::new(
            ErrorCategory::InvalidArgument,
            "callback must not be NULL",
            ffi::SQLITE_MISUSE,
        )
    })?;
    // SAFETY: caller guarantees callback/user_data originate from the active ABI call;
    // payload bytes remain live for the duration of this callback invocation.
    let cancelled = unsafe { callback(user_data, kind, payload.as_ptr(), payload.len()) };
    if cancelled != 0 {
        return Err(LithographError::new(
            ErrorCategory::Resource,
            "query interrupted by event callback",
            ffi::SQLITE_INTERRUPT,
        ));
    }
    Ok(())
}

/// Validates one Cypher query through the stable native ABI v1.
///
/// # Safety
///
/// The pointer and ownership rules are the same as [`lithograph_v1_execute`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lithograph_v1_validate(
    db: *mut ffi::sqlite3,
    query: *const c_char,
    query_len: usize,
    error_json: *mut *mut c_char,
) -> c_int {
    let operation = || {
        // SAFETY: the public ABI guarantees the query buffer is readable for
        // the duration of this invocation.
        unsafe { native_validate_impl(db, query, query_len) }
    };
    // SAFETY: the public ABI contract guarantees the query buffer is readable
    // and a non-NULL `error_json` points to writable out-parameter storage.
    unsafe { native_boundary(db, error_json, operation) }
}

unsafe fn native_validate_impl(
    db: *mut ffi::sqlite3,
    query: *const c_char,
    query_len: usize,
) -> LithographResult<()> {
    require_native_db(db)?;
    require_native_connection_registered(db)?;
    if explicit_transaction_state(db).is_some() {
        return Err(transaction_boundary_error(
            "an explicit Lithograph transaction is active on this connection",
        ));
    }
    // SAFETY: the public ABI requires this buffer to remain readable for the
    // duration of the call; `input_utf8` copies before returning.
    let query = unsafe { input_utf8(query, query_len, "query")? };
    if query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty"));
    }
    // SAFETY: registration proves `db` is a live SQLite connection containing
    // this extension; rusqlite borrows the handle without taking ownership.
    let connection = unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "invalid SQLite connection"))?;
    require_no_explicit_transaction(&connection)?;
    require_initialized(&connection)?;
    validate_cypher(&query)?;
    Ok(())
}

fn require_native_db(db: *mut ffi::sqlite3) -> LithographResult<()> {
    if db.is_null() {
        Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            "db must not be NULL",
            ffi::SQLITE_MISUSE,
        ))
    } else {
        Ok(())
    }
}

fn require_native_connection_registered(db: *mut ffi::sqlite3) -> LithographResult<()> {
    if !native_connection_registered(db) {
        return Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            "target SQLite connection has not loaded the Lithograph extension",
            ffi::SQLITE_MISUSE,
        ));
    }
    Ok(())
}

fn native_connection_registered(db: *mut ffi::sqlite3) -> bool {
    registered_connections()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(&(db as usize))
}

pub(super) unsafe fn input_utf8(
    pointer: *const c_char,
    length: usize,
    name: &str,
) -> LithographResult<String> {
    if pointer.is_null() {
        if length == 0 {
            return Ok(String::new());
        }
        return Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            format!("{name} pointer is NULL with a non-zero length"),
            ffi::SQLITE_MISUSE,
        ));
    }
    // SAFETY: callers of this unsafe helper guarantee the non-NULL pointer is
    // readable for `length` bytes; this function copies before returning.
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| LithographError::invalid_argument(format!("{name} must contain valid UTF-8")))
}

unsafe fn clear_error_out(error_json: *mut *mut c_char) {
    if !error_json.is_null() {
        // SAFETY: callers guarantee a non-NULL pointer references writable ABI
        // out-parameter storage.
        unsafe { *error_json = ptr::null_mut() };
    }
}

unsafe fn write_native_error(error_json: *mut *mut c_char, error: &LithographError) -> c_int {
    if !error_json.is_null()
        && let Ok(payload) = CString::new(error.to_json())
    {
        // SAFETY: callers guarantee a non-NULL pointer references writable ABI
        // out-parameter storage; ownership of the CString is transferred out.
        unsafe { *error_json = payload.into_raw() };
    }
    error.sqlite_code
}

#[derive(Debug)]
struct TxBeginOptions {
    branch: Option<String>,
    expected_head: Option<storage::HashId>,
    author: Option<String>,
    message: Option<String>,
}

/// Starts one connection-scoped Native explicit transaction.
///
/// # Safety
///
/// Pointer/ownership rules follow [`lithograph_v1_execute`]. Successful
/// `result_json` and failed `error_json` allocations are released with
/// [`lithograph_v1_free`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lithograph_v1_tx_begin(
    db: *mut ffi::sqlite3,
    options_json: *const c_char,
    options_len: usize,
    result_json: *mut *mut c_char,
    error_json: *mut *mut c_char,
) -> c_int {
    let operation = || {
        // SAFETY: the public ABI keeps the input buffer readable for this call.
        unsafe { native_tx_begin_impl(db, NativeTextInput::new(options_json, options_len)) }
    };
    // SAFETY: non-NULL out-pointers are writable for this invocation.
    unsafe { native_result_boundary(db, result_json, error_json, operation) }
}

unsafe fn native_tx_begin_impl(
    db: *mut ffi::sqlite3,
    options_json: NativeTextInput,
) -> LithographResult<String> {
    require_native_db(db)?;
    require_native_connection_registered(db)?;
    if explicit_transaction_state(db).is_some() {
        return Err(transaction_boundary_error(
            "an explicit Lithograph transaction is already active on this connection",
        ));
    }
    // SAFETY: public ABI guarantees the optional options buffer is readable.
    let options_text =
        unsafe { input_utf8(options_json.pointer, options_json.length, "options_json")? };
    let options = parse_tx_begin_options(&options_text)?;
    // SAFETY: registration proves this is a live SQLite connection borrowed
    // without ownership transfer.
    let connection = unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "invalid SQLite connection"))?;
    let metadata = require_initialized(&connection)?;
    require_current_storage_format(&metadata)?;
    require_no_active_readers(&connection)?;
    if !connection.is_autocommit() {
        return Err(transaction_boundary_error(
            "Native explicit transaction requires SQLite autocommit mode",
        ));
    }
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .map_err(|error| map_sqlite_error(error, "failed to begin explicit transaction"))?;

    let begin = catch_unwind(AssertUnwindSafe(|| {
        begin_explicit_transaction_state(db, &connection, options)
    }));
    match begin {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) => fail_closed_tx_with_connection(db, &connection, error),
        Err(_) => fail_closed_tx_with_connection(
            db,
            &connection,
            LithographError::internal("panic while starting explicit transaction"),
        ),
    }
}

pub(super) unsafe fn sql_tx_begin(
    db: *mut ffi::sqlite3,
    options_json: &str,
    validate_result: impl FnOnce(&str) -> LithographResult<()>,
) -> LithographResult<String> {
    let mut began = false;
    let operation = || {
        // SAFETY: both slices point to the live Rust input for this synchronous call.
        let result = unsafe {
            native_tx_begin_impl(
                db,
                NativeTextInput::new(options_json.as_ptr().cast::<c_char>(), options_json.len()),
            )
        }?;
        began = true;
        validate_result(&result)?;
        Ok(result)
    };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(result)) => Ok(result),
        Ok(Err(error)) if began => {
            // SAFETY: `db` is the live handle of the scalar callback. Cleanup
            // applies only after this invocation successfully completed BEGIN.
            Err(unsafe { cleanup_active_transaction_after_error(db, error) })
        }
        Ok(Err(error)) => Err(error),
        Err(_) => {
            let error =
                LithographError::internal("panic while starting explicit transaction SQL callback");
            if began {
                // SAFETY: `db` is the live handle of the active scalar callback.
                Err(unsafe { cleanup_active_transaction_after_error(db, error) })
            } else {
                Err(error)
            }
        }
    }
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
        started_at_micros: native_now_micros()?,
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

/// Executes one query inside the active connection-scoped explicit transaction.
///
/// # Safety
///
/// Pointer/ownership rules follow [`lithograph_v1_execute`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lithograph_v1_tx_execute(
    db: *mut ffi::sqlite3,
    query: *const c_char,
    query_len: usize,
    params_json: *const c_char,
    params_len: usize,
    options_json: *const c_char,
    options_len: usize,
    callback: LithographEventCallbackV1,
    user_data: *mut c_void,
    error_json: *mut *mut c_char,
) -> c_int {
    let operation = || {
        // SAFETY: pointer/length validity is guaranteed for this invocation.
        unsafe {
            native_tx_execute_impl(
                db,
                NativeTextInput::new(query, query_len),
                NativeTextInput::new(params_json, params_len),
                NativeTextInput::new(options_json, options_len),
                callback,
                user_data,
            )
        }
    };
    // SAFETY: non-NULL error out-pointer is writable for this invocation.
    unsafe { native_tx_boundary(db, error_json, operation) }
}

unsafe fn native_tx_execute_impl(
    db: *mut ffi::sqlite3,
    query: NativeTextInput,
    params_json: NativeTextInput,
    options_json: NativeTextInput,
    callback: LithographEventCallbackV1,
    user_data: *mut c_void,
) -> LithographResult<()> {
    // SAFETY: the Native ABI caller owns `db` for this invocation.
    let (state, connection) = unsafe { explicit_transaction_context(db)? };

    // SAFETY: all Native buffers and callback remain valid for this invocation.
    let execution_result = unsafe {
        run_explicit_transaction_query(
            db,
            &connection,
            &state,
            query,
            params_json,
            options_json,
            callback,
            user_data,
        )
    };
    match execution_result {
        Ok(()) => Ok(()),
        Err(error) => fail_closed_tx_with_connection(db, &connection, error),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the shared transaction execution boundary keeps the three input buffers and event callback explicit"
)]
pub(super) unsafe fn sql_tx_execute(
    db: *mut ffi::sqlite3,
    query: &str,
    params_json: &str,
    options_json: &str,
    callback: LithographEventCallbackV1,
    user_data: *mut c_void,
) -> LithographResult<()> {
    let operation = || {
        // SAFETY: all Rust strings and the callback state remain live for this
        // synchronous invocation.
        unsafe {
            native_tx_execute_impl(
                db,
                NativeTextInput::new(query.as_ptr().cast::<c_char>(), query.len()),
                NativeTextInput::new(params_json.as_ptr().cast::<c_char>(), params_json.len()),
                NativeTextInput::new(options_json.as_ptr().cast::<c_char>(), options_json.len()),
                callback,
                user_data,
            )
        }
    };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => {
            let error = LithographError::internal(
                "panic while executing explicit transaction SQL callback",
            );
            // SAFETY: `db` is the live handle of the active scalar callback.
            Err(unsafe { cleanup_active_transaction_after_error(db, error) })
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the Native ABI query boundary keeps caller-owned buffers and callback explicit"
)]
unsafe fn run_explicit_transaction_query(
    db: *mut ffi::sqlite3,
    connection: &Connection,
    state: &ExplicitTransactionState,
    query: NativeTextInput,
    params_json: NativeTextInput,
    options_json: NativeTextInput,
    callback: LithographEventCallbackV1,
    user_data: *mut c_void,
) -> LithographResult<()> {
    if callback.is_none() {
        return Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            "callback must not be NULL",
            ffi::SQLITE_MISUSE,
        ));
    }
    // SAFETY: the surrounding ABI call guarantees readable buffers.
    let inputs = unsafe { decode_native_execute_inputs(query, params_json, options_json)? };
    let mut execution = prepare_explicit_execution(connection, state, &inputs)?;
    let mutated = execution.is_write();
    // SAFETY: callback and user_data are borrowed from the surrounding ABI invocation.
    unsafe { emit_execution_events(connection, &mut execution, callback, user_data) }?;
    if mutated {
        mark_explicit_transaction_mutated(db)?;
    }
    Ok(())
}

fn prepare_explicit_execution(
    connection: &Connection,
    state: &ExplicitTransactionState,
    inputs: &NativeExecuteInputs,
) -> LithographResult<execution::AdapterExecution> {
    if inputs.query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty"));
    }
    let tx_options = tx_execute_options(&inputs.options_json, &state.branch)?;
    let mut execution = execution::AdapterExecution::prepare(
        connection,
        &inputs.query,
        &inputs.params_json,
        &tx_options,
    )?;
    if execution.requires_transaction_boundary()
        || execution.has_external_io()
        || execution.has_version_operation()
    {
        return Err(transaction_boundary_error(
            "explicit transaction cannot execute Version Procedures, external I/O, or transaction-owning Cypher",
        ));
    }
    execution.set_transaction_time_micros(state.started_at_micros)?;
    execution.suppress_summary_commit();
    Ok(execution)
}

fn mark_explicit_transaction_mutated(db: *mut ffi::sqlite3) -> LithographResult<()> {
    update_explicit_transaction_state(db, |state| {
        state.mutated = true;
        Ok(())
    })
}

/// Commits the active explicit transaction and returns the single durable
/// Commit identity plus transaction-level final-delta counters.
///
/// # Safety
///
/// Output ownership follows [`lithograph_v1_tx_begin`].
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lithograph_v1_tx_commit(
    db: *mut ffi::sqlite3,
    result_json: *mut *mut c_char,
    error_json: *mut *mut c_char,
) -> c_int {
    let operation = || {
        // SAFETY: the caller-owned SQLite handle remains valid for this ABI invocation.
        unsafe { native_tx_commit_impl(db) }
    };
    // SAFETY: non-NULL out-pointers are writable for this invocation.
    unsafe { native_tx_result_boundary(db, result_json, error_json, operation) }
}

unsafe fn native_tx_commit_impl(db: *mut ffi::sqlite3) -> LithographResult<String> {
    // SAFETY: the Native ABI caller owns `db` for this invocation.
    unsafe { tx_commit_impl(db, |_connection, _result| Ok(())) }
}

pub(super) unsafe fn sql_tx_commit(db: *mut ffi::sqlite3) -> LithographResult<String> {
    let operation = || {
        // SAFETY: the scalar callback owns the live connection handle for this invocation.
        unsafe {
            tx_commit_impl(db, |connection, result| {
                execution::ensure_scalar_result_fits(connection, result)
            })
        }
    };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => {
            let error = LithographError::internal(
                "panic while committing explicit transaction SQL callback",
            );
            // SAFETY: `db` is the live handle of the active scalar callback.
            Err(unsafe { cleanup_active_transaction_after_error(db, error) })
        }
    }
}

unsafe fn tx_commit_impl(
    db: *mut ffi::sqlite3,
    validate_result: impl FnOnce(&Connection, &str) -> LithographResult<()>,
) -> LithographResult<String> {
    // SAFETY: the Native ABI caller owns `db` for this invocation.
    let (state, connection) = unsafe { explicit_transaction_context(db)? };

    let result = finalize_explicit_transaction(&connection, &state);
    let result = match result {
        Ok(result) => result,
        Err(error) => return fail_closed_tx_with_connection(db, &connection, error),
    };
    if let Err(error) = validate_result(&connection, &result) {
        return fail_closed_tx_with_connection(db, &connection, error);
    }
    if let Err(error) = connection.execute_batch("COMMIT") {
        return fail_closed_tx_with_connection(
            db,
            &connection,
            map_sqlite_error(error, "failed to commit explicit transaction"),
        );
    }
    clear_explicit_transaction_state(db);
    Ok(result)
}

unsafe fn explicit_transaction_context(
    db: *mut ffi::sqlite3,
) -> LithographResult<(ExplicitTransactionState, Connection)> {
    require_native_db(db)?;
    require_native_connection_registered(db)?;
    let state = explicit_transaction_state(db)
        .ok_or_else(|| transaction_misuse("no active explicit Lithograph transaction"))?;
    // SAFETY: registration proves this is a live SQLite connection borrowed
    // without ownership transfer.
    let connection = unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "invalid SQLite connection"))?;
    Ok((state, connection))
}

/// Aborts the active explicit transaction.
///
/// # Safety
///
/// Pointer/ownership rules follow the other Native v1 functions.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lithograph_v1_tx_abort(
    db: *mut ffi::sqlite3,
    error_json: *mut *mut c_char,
) -> c_int {
    let operation = || {
        // SAFETY: the caller-owned SQLite handle remains valid for this ABI invocation.
        unsafe { native_tx_abort_impl(db) }
    };
    // SAFETY: non-NULL error out-pointer is writable for this invocation.
    unsafe { native_tx_boundary(db, error_json, operation) }
}

unsafe fn native_tx_abort_impl(db: *mut ffi::sqlite3) -> LithographResult<()> {
    require_native_db(db)?;
    require_native_connection_registered(db)?;
    if explicit_transaction_state(db).is_none() {
        return Err(transaction_misuse(
            "no active explicit Lithograph transaction",
        ));
    }
    // SAFETY: registration proves this is a live SQLite connection borrowed
    // without ownership transfer.
    let connection = unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "invalid SQLite connection"))?;
    rollback_explicit_transaction(db, &connection)
}

pub(super) unsafe fn sql_tx_abort(db: *mut ffi::sqlite3) -> LithographResult<()> {
    let operation = || {
        // SAFETY: the scalar callback owns the live connection handle.
        unsafe { native_tx_abort_impl(db) }
    };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => {
            let error =
                LithographError::internal("panic while aborting explicit transaction SQL callback");
            // SAFETY: `db` is the live handle of the active scalar callback.
            Err(unsafe { cleanup_active_transaction_after_error(db, error) })
        }
    }
}

pub(super) fn fail_closed_sql_transaction<T>(
    connection: &Connection,
    error: LithographError,
) -> LithographResult<T> {
    // SAFETY: `connection` is borrowed from the active SQLite scalar callback.
    let db = unsafe { connection.handle() };
    if explicit_transaction_state(db).is_none() {
        return Err(error);
    }
    fail_closed_tx_with_connection(db, connection, error)
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
    // Each staged execution already validates the graph/schema state before
    // advancing the staged Branch. Composing the touched first-parent Layer
    // slots back to the transaction base avoids duplicate O(total graph)
    // materialization at final Commit time.
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
        committed_at: native_now_micros()?,
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

fn tx_execute_options(text: &str, branch: &str) -> LithographResult<String> {
    let value: Value = serde_json::from_str(if text.is_empty() { "{}" } else { text })
        .map_err(|_| LithographError::invalid_argument("options must contain valid JSON"))?;
    let object = value
        .as_object()
        .ok_or_else(|| LithographError::invalid_argument("options must be a JSON object"))?;
    for key in object.keys() {
        if key != "graphView" {
            return Err(LithographError::invalid_argument(format!(
                "explicit transaction tx_execute does not accept option {key}"
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

fn native_now_micros() -> LithographResult<i64> {
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
    rollback
}

unsafe fn native_tx_boundary(
    db: *mut ffi::sqlite3,
    error_json: *mut *mut c_char,
    operation: impl FnOnce() -> LithographResult<()>,
) -> c_int {
    let _guard = NativeDbMutexGuard::for_registered(db);
    // SAFETY: callers guarantee a non-NULL pointer references writable ABI
    // out-parameter storage.
    unsafe { clear_error_out(error_json) };
    let result = match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => Err(LithographError::internal(
            "panic while executing explicit transaction ABI call",
        )),
    };
    match result {
        Ok(()) => ffi::SQLITE_OK,
        Err(error) => {
            // SAFETY: `db` originates from this ABI invocation; cleanup only
            // borrows it if a Lithograph explicit transaction is still active.
            let error = unsafe { cleanup_active_transaction_after_error(db, error) };
            // SAFETY: caller owns writable error out-parameter storage.
            // SAFETY: the public ABI allows a writable error out-pointer or NULL.
            unsafe { write_native_error(error_json, &error) }
        }
    }
}

unsafe fn cleanup_active_transaction_after_error(
    db: *mut ffi::sqlite3,
    error: LithographError,
) -> LithographError {
    if db.is_null() || explicit_transaction_state(db).is_none() {
        return error;
    }
    if let Err(registration_error) = require_native_connection_registered(db) {
        return LithographError::internal(format!(
            "explicit transaction cleanup could not validate the connection after {}: {}",
            error.message, registration_error.message
        ));
    }
    // SAFETY: registration proves the handle is a live SQLite connection and
    // rusqlite borrows it without taking ownership.
    let connection = match unsafe { Connection::from_handle(db) } {
        Ok(connection) => connection,
        Err(sqlite) => {
            return LithographError::internal(format!(
                "explicit transaction cleanup could not access the SQLite connection after {}: {}",
                error.message, sqlite
            ));
        }
    };
    match rollback_explicit_transaction(db, &connection) {
        Ok(()) => error,
        Err(cleanup) => LithographError::internal(format!(
            "explicit transaction cleanup failed after {}: {}",
            error.message, cleanup.message
        )),
    }
}

unsafe fn native_tx_result_boundary(
    db: *mut ffi::sqlite3,
    result_json: *mut *mut c_char,
    error_json: *mut *mut c_char,
    operation: impl FnOnce() -> LithographResult<String>,
) -> c_int {
    let _guard = NativeDbMutexGuard::for_registered(db);
    // SAFETY: the public ABI allows a writable result out-pointer or NULL.
    unsafe { clear_error_out(result_json) };
    // SAFETY: the public ABI allows a writable error out-pointer or NULL.
    unsafe { clear_error_out(error_json) };
    let result = match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => Err(LithographError::internal(
            "panic while executing explicit transaction ABI call",
        )),
    };
    match result {
        Ok(result) => {
            if !result_json.is_null() {
                match CString::new(result) {
                    Ok(payload) => {
                        // SAFETY: `result_json` was checked non-NULL and is writable.
                        unsafe { *result_json = payload.into_raw() };
                    }
                    Err(_) => {
                        let error =
                            LithographError::internal("result JSON contains an embedded NUL");
                        // SAFETY: clean an active explicit transaction before
                        // returning an allocation/encoding failure.
                        let error = unsafe { cleanup_active_transaction_after_error(db, error) };
                        // SAFETY: the public ABI allows a writable error out-pointer or NULL.
                        return unsafe { write_native_error(error_json, &error) };
                    }
                }
            }
            ffi::SQLITE_OK
        }
        Err(error) => {
            // SAFETY: same ABI handle lifetime as this boundary call.
            let error = unsafe { cleanup_active_transaction_after_error(db, error) };
            // SAFETY: the public ABI allows a writable error out-pointer or NULL.
            unsafe { write_native_error(error_json, &error) }
        }
    }
}

unsafe fn native_result_boundary(
    db: *mut ffi::sqlite3,
    result_json: *mut *mut c_char,
    error_json: *mut *mut c_char,
    operation: impl FnOnce() -> LithographResult<String>,
) -> c_int {
    let _guard = NativeDbMutexGuard::for_registered(db);
    // SAFETY: the public ABI allows a writable result out-pointer or NULL.
    unsafe { clear_error_out(result_json) };
    // SAFETY: the public ABI allows a writable error out-pointer or NULL.
    unsafe { clear_error_out(error_json) };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(result)) => {
            if !result_json.is_null() {
                match CString::new(result) {
                    Ok(payload) => {
                        // SAFETY: `result_json` was checked non-NULL and is writable.
                        unsafe { *result_json = payload.into_raw() };
                    }
                    Err(_) => {
                        // SAFETY: the public ABI allows a writable error out-pointer or NULL.
                        return unsafe {
                            write_native_error(
                                error_json,
                                &LithographError::internal("result JSON contains an embedded NUL"),
                            )
                        };
                    }
                }
            }
            ffi::SQLITE_OK
        }
        Ok(Err(error)) => {
            // SAFETY: the public ABI allows a writable error out-pointer or NULL.
            unsafe { write_native_error(error_json, &error) }
        }
        Err(_) => {
            // SAFETY: the public ABI allows a writable error out-pointer or NULL.
            unsafe {
                write_native_error(
                    error_json,
                    &LithographError::internal("panic while executing native ABI call"),
                )
            }
        }
    }
}

/// Releases memory returned through the native ABI v1.
///
/// # Safety
///
/// `pointer` must be NULL or a pointer returned by this Lithograph shared
/// library through an ABI v1 allocation result such as `error_json`.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn lithograph_v1_free(pointer: *mut c_void) {
    if !pointer.is_null() {
        // SAFETY: the ABI contract requires `pointer` to originate from
        // `CString::into_raw` in this shared library and to be freed once.
        unsafe {
            drop(CString::from_raw(pointer.cast::<c_char>()));
        }
    }
}
