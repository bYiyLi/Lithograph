use super::*;

type ScalarCallback = unsafe extern "C" fn(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
);

pub(super) fn register_scalar_functions(db: &Connection) -> SqliteResult<()> {
    let direct = ffi::SQLITE_UTF8 | ffi::SQLITE_DIRECTONLY;
    let innocuous = ffi::SQLITE_UTF8 | ffi::SQLITE_INNOCUOUS;
    register_scalar(db, c"lithograph_init", 0, direct, scalar_init)?;
    register_scalar(db, c"lithograph", -1, direct, scalar_execute)?;
    register_scalar(db, c"lithograph_tx_begin", 1, direct, scalar_tx_begin)?;
    register_scalar(db, c"lithograph_tx_execute", -1, direct, scalar_tx_execute)?;
    register_scalar(db, c"lithograph_tx_commit", 0, direct, scalar_tx_commit)?;
    register_scalar(db, c"lithograph_tx_abort", 0, direct, scalar_tx_abort)?;
    register_scalar(db, c"lithograph_validate", 1, innocuous, scalar_validate)?;
    register_scalar(db, c"lithograph_version", 0, innocuous, scalar_version)?;
    register_scalar(
        db,
        c"lithograph_integrity_check",
        0,
        innocuous,
        scalar_integrity_check,
    )?;
    Ok(())
}

fn register_scalar(
    db: &Connection,
    name: &CStr,
    argc: c_int,
    flags: c_int,
    callback: ScalarCallback,
) -> SqliteResult<()> {
    // SAFETY: `db` is a live rusqlite connection for the registration call.
    let handle = unsafe { db.handle() };
    // SAFETY: `handle` is live, names/callbacks are static, and no user-data
    // destructor is required because the user-data pointer is NULL.
    let rc = unsafe {
        ffi::sqlite3_create_function_v2(
            handle,
            name.as_ptr(),
            argc,
            flags,
            ptr::null_mut(),
            Some(callback),
            None,
            None,
            None,
        )
    };
    if rc == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(SqliteError::SqliteFailure(ffi::Error::new(rc), None))
    }
}

unsafe extern "C" fn scalar_init(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv` for
    // this callback invocation.
    unsafe {
        run_scalar(context, argc, argv, |_args, connection| {
            require_no_explicit_transaction(connection)?;
            with_savepoint(connection, initialize).map(|value| value.to_string())
        });
    }
}

unsafe extern "C" fn scalar_execute(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv` for
    // this callback invocation.
    unsafe {
        run_scalar(context, argc, argv, |args, connection| {
            require_no_explicit_transaction(connection)?;
            let (query, params, options) = execution_args(args)?;
            validate_query_ready(connection, &query)?;
            execution::scalar_result(connection, &query, &params, &options)
        });
    }
}

unsafe extern "C" fn scalar_tx_begin(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let operation = |args: &ScalarArgs, connection: &Connection| {
        let options = args.text(0, "options_json must be JSON TEXT")?;
        // SAFETY: `connection` borrows the live callback connection.
        let db = unsafe { connection.handle() };
        // SAFETY: `db`, `options`, and the validation closure remain live for
        // this synchronous callback invocation.
        unsafe {
            native::sql_tx_begin(db, &options, |result| {
                execution::ensure_scalar_result_fits(connection, result)
            })
        }
    };
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv`.
    unsafe { run_scalar(context, argc, argv, operation) };
}

unsafe extern "C" fn scalar_tx_execute(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let operation = |args: &ScalarArgs, connection: &Connection| {
        let (query, params, options) = match tx_execution_args(args) {
            Ok(inputs) => inputs,
            Err(error) => {
                return native::fail_closed_sql_transaction(connection, error);
            }
        };
        // SAFETY: `connection` borrows the live callback connection.
        let db = unsafe { connection.handle() };
        let mut collector = TxScalarEventCollector::new(db);
        // SAFETY: all inputs and collector state remain live for the
        // synchronous transaction execution.
        let execute = unsafe {
            native::sql_tx_execute(
                db,
                &query,
                &params,
                &options,
                Some(collect_tx_scalar_event),
                (&raw mut collector).cast::<c_void>(),
            )
        };
        match execute {
            Ok(()) => match collector.finish() {
                Ok(result) => Ok(result),
                Err(error) => native::fail_closed_sql_transaction(connection, error),
            },
            Err(error) => Err(collector.error.take().unwrap_or(error)),
        }
    };
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv`.
    unsafe { run_scalar(context, argc, argv, operation) };
}

unsafe extern "C" fn scalar_tx_commit(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let operation = |_args: &ScalarArgs, connection: &Connection| {
        // SAFETY: `connection` borrows the live callback connection.
        let db = unsafe { connection.handle() };
        // SAFETY: `db` remains live for this synchronous callback invocation.
        unsafe { native::sql_tx_commit(db) }
    };
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv`.
    unsafe { run_scalar(context, argc, argv, operation) };
}

unsafe extern "C" fn scalar_tx_abort(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    let operation = |_args: &ScalarArgs, connection: &Connection| {
        // SAFETY: `connection` borrows the live callback connection.
        let db = unsafe { connection.handle() };
        // SAFETY: `db` remains live for this synchronous callback invocation.
        unsafe { native::sql_tx_abort(db) }?;
        let result = json!({"aborted": true}).to_string();
        execution::ensure_scalar_result_fits(connection, &result)?;
        Ok(result)
    };
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv`.
    unsafe { run_scalar(context, argc, argv, operation) };
}

unsafe extern "C" fn scalar_validate(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv` for
    // this callback invocation.
    unsafe {
        run_scalar(context, argc, argv, |args, connection| {
            require_no_explicit_transaction(connection)?;
            let query = args.text(0, "query must be TEXT")?;
            validate_query_ready(connection, &query)?;
            validate_cypher(&query).map(|value| value.to_string())
        });
    }
}

unsafe extern "C" fn scalar_version(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv`.
    unsafe {
        run_scalar(context, argc, argv, |_args, connection| {
            version_json(connection).map(|value| value.to_string())
        });
    }
}

unsafe extern "C" fn scalar_integrity_check(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) {
    // SAFETY: SQLite owns `context` and supplies `argc` entries in `argv`.
    unsafe {
        run_scalar(context, argc, argv, |_args, connection| {
            require_no_explicit_transaction(connection)?;
            metadata_integrity_json(connection).map(|value| value.to_string())
        });
    }
}

unsafe fn run_scalar(
    context: *mut ffi::sqlite3_context,
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
    operation: impl FnOnce(&ScalarArgs, &Connection) -> LithographResult<String>,
) {
    let boundary_operation = || {
        // SAFETY: caller guarantees SQLite supplied `argc` valid entries in
        // `argv` for this active callback.
        let args = unsafe { scalar_args(argc, argv)? };
        // SAFETY: caller guarantees `context` belongs to this active callback.
        let connection = unsafe { scalar_connection(context)? };
        operation(&args, &connection)
    };
    // SAFETY: caller guarantees `context` belongs to this active callback.
    unsafe { scalar_boundary(context, boundary_operation) };
}

fn validate_query_ready(connection: &Connection, query: &str) -> LithographResult<()> {
    if query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty"));
    }
    require_initialized(connection)?;
    Ok(())
}

fn execution_args(args: &ScalarArgs) -> LithographResult<(String, String, String)> {
    if !(1..=3).contains(&args.len()) {
        return Err(LithographError::invalid_argument(
            "lithograph() expects query [, params [, options]]",
        ));
    }
    // SAFETY: `args` contains SQLite-owned values for this callback.
    let query = args.text(0, "query must be TEXT")?;
    let params = if args.len() >= 2 {
        // SAFETY: `args` contains SQLite-owned values for this callback.
        args.text(1, "params must be JSON TEXT")?
    } else {
        "{}".to_owned()
    };
    let options = if args.len() >= 3 {
        // SAFETY: `args` contains SQLite-owned values for this callback.
        args.text(2, "options must be JSON TEXT")?
    } else {
        "{}".to_owned()
    };
    Ok((query, params, options))
}

fn tx_execution_args(args: &ScalarArgs) -> LithographResult<(String, String, String)> {
    if !(1..=3).contains(&args.len()) {
        return Err(LithographError::invalid_argument(
            "lithograph_tx_execute() expects query [, params_json [, options_json]]",
        ));
    }
    let query = args.text(0, "query must be TEXT")?;
    let params = if args.len() >= 2 {
        args.text(1, "params_json must be JSON TEXT")?
    } else {
        "{}".to_owned()
    };
    let options = if args.len() >= 3 {
        args.text(2, "options_json must be JSON TEXT")?
    } else {
        "{}".to_owned()
    };
    Ok((query, params, options))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TxScalarEventStage {
    Columns,
    Rows,
    Done,
}

struct TxScalarEventCollector {
    db: *mut ffi::sqlite3,
    stage: TxScalarEventStage,
    columns: Option<Value>,
    rows: Vec<Value>,
    result: Option<String>,
    error: Option<LithographError>,
}

impl TxScalarEventCollector {
    fn new(db: *mut ffi::sqlite3) -> Self {
        Self {
            db,
            stage: TxScalarEventStage::Columns,
            columns: None,
            rows: Vec::new(),
            result: None,
            error: None,
        }
    }

    fn collect(
        &mut self,
        kind: native::LithographEventKindV1,
        payload: &[u8],
    ) -> LithographResult<()> {
        let value: Value = serde_json::from_slice(payload).map_err(|error| {
            LithographError::internal(format!(
                "failed to decode explicit transaction result event: {error}"
            ))
        })?;
        match (self.stage, kind) {
            (TxScalarEventStage::Columns, native::LithographEventKindV1::Columns) => {
                let valid = value
                    .as_array()
                    .is_some_and(|columns| columns.iter().all(Value::is_string));
                if !valid {
                    return Err(LithographError::internal(
                        "explicit transaction COLUMNS event is not a string array",
                    ));
                }
                self.columns = Some(value);
                self.stage = TxScalarEventStage::Rows;
            }
            (TxScalarEventStage::Rows, native::LithographEventKindV1::Row) => {
                if !value.is_array() {
                    return Err(LithographError::internal(
                        "explicit transaction ROW event is not an array",
                    ));
                }
                self.rows.push(value);
            }
            (TxScalarEventStage::Rows, native::LithographEventKindV1::Summary) => {
                if !value.is_object() {
                    return Err(LithographError::internal(
                        "explicit transaction SUMMARY event is not an object",
                    ));
                }
                let columns = self.columns.take().ok_or_else(|| {
                    LithographError::internal(
                        "explicit transaction result is missing COLUMNS event",
                    )
                })?;
                let result = json!({
                    "columns": columns,
                    "rows": std::mem::take(&mut self.rows),
                    "summary": value,
                })
                .to_string();
                execution::ensure_scalar_result_fits_handle(self.db, &result)?;
                self.result = Some(result);
                self.stage = TxScalarEventStage::Done;
            }
            _ => {
                return Err(LithographError::internal(
                    "explicit transaction result events are out of order",
                ));
            }
        }
        Ok(())
    }

    fn finish(self) -> LithographResult<String> {
        if let Some(error) = self.error {
            return Err(error);
        }
        if self.stage != TxScalarEventStage::Done {
            return Err(LithographError::internal(
                "explicit transaction result is missing SUMMARY event",
            ));
        }
        self.result.ok_or_else(|| {
            LithographError::internal("explicit transaction result envelope disappeared")
        })
    }
}

unsafe extern "C" fn collect_tx_scalar_event(
    user_data: *mut c_void,
    kind: native::LithographEventKindV1,
    json: *const u8,
    json_len: usize,
) -> c_int {
    if user_data.is_null() || (json.is_null() && json_len != 0) {
        return 1;
    }
    // SAFETY: user_data points to the collector owned by the synchronous
    // scalar invocation; payload bytes remain valid for this callback only.
    let collector = unsafe { &mut *user_data.cast::<TxScalarEventCollector>() };
    let collect = || {
        let payload = if json_len == 0 {
            &[][..]
        } else {
            // SAFETY: the Native event contract provides `json_len` readable bytes.
            unsafe { std::slice::from_raw_parts(json, json_len) }
        };
        collector.collect(kind, payload)
    };
    match catch_unwind(AssertUnwindSafe(collect)) {
        Ok(Ok(())) => 0,
        Ok(Err(error)) => {
            collector.error = Some(error);
            1
        }
        Err(_) => {
            collector.error = Some(LithographError::internal(
                "panic while collecting explicit transaction result events",
            ));
            1
        }
    }
}

pub(super) fn catch_scalar_operation<T>(
    operation: impl FnOnce() -> LithographResult<T>,
) -> LithographResult<T> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => Err(LithographError::internal(
            "panic while executing a SQLite scalar callback",
        )),
    }
}

unsafe fn scalar_boundary(
    context: *mut ffi::sqlite3_context,
    operation: impl FnOnce() -> LithographResult<String>,
) {
    match catch_scalar_operation(operation) {
        Ok(value) => {
            // SAFETY: SQLITE_TRANSIENT makes SQLite copy the Rust bytes before
            // the callback returns.
            unsafe {
                ffi::sqlite3_result_text64(
                    context,
                    value.as_ptr().cast::<c_char>(),
                    value.len() as u64,
                    ffi::SQLITE_TRANSIENT(),
                    ffi::SQLITE_UTF8 as u8,
                );
            }
        }
        Err(error) => {
            // SAFETY: caller guarantees `context` belongs to the active callback.
            unsafe { scalar_error(context, &error) };
        }
    }
}

unsafe fn scalar_error(context: *mut ffi::sqlite3_context, error: &LithographError) {
    let message = format!("LITHOGRAPH_{}: {}", error.category.as_str(), error.message);
    let Ok(length) = c_int::try_from(message.len()) else {
        let fallback = b"LITHOGRAPH_RESOURCE_ERROR: scalar error message is too large";
        // SAFETY: sqlite3_result_error copies the fallback before returning.
        unsafe {
            ffi::sqlite3_result_error(
                context,
                fallback.as_ptr().cast::<c_char>(),
                fallback.len() as c_int,
            );
        }
        // SAFETY: `context` belongs to the active callback.
        unsafe { ffi::sqlite3_result_error_code(context, ffi::SQLITE_TOOBIG) };
        return;
    };
    // sqlite3_result_error() resets the result to SQLITE_ERROR, so assign the
    // public primary code afterwards. This ordering is part of the SQL ABI.
    // SAFETY: SQLite copies `message` before this callback returns.
    unsafe { ffi::sqlite3_result_error(context, message.as_ptr().cast::<c_char>(), length) };
    // SAFETY: `context` belongs to the active callback.
    unsafe { ffi::sqlite3_result_error_code(context, error.sqlite_code) };
}

unsafe fn scalar_connection(context: *mut ffi::sqlite3_context) -> LithographResult<Connection> {
    if context.is_null() {
        return Err(LithographError::internal("SQLite scalar context is NULL"));
    }
    // SAFETY: SQLite owns this context for the callback duration.
    let db = unsafe { ffi::sqlite3_context_db_handle(context) };
    if db.is_null() {
        return Err(LithographError::internal(
            "SQLite scalar context has no database connection",
        ));
    }
    // SAFETY: the wrapper borrows the callback's live sqlite3* and does not
    // take ownership of it.
    unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "failed to access the SQLite connection"))
}

struct ScalarArgs {
    values: Vec<*mut ffi::sqlite3_value>,
}

impl ScalarArgs {
    fn len(&self) -> usize {
        self.values.len()
    }

    fn text(&self, index: usize, invalid_type_message: &str) -> LithographResult<String> {
        // SAFETY: `ScalarArgs` is only constructed by `scalar_args`, which
        // copies SQLite-owned callback values that remain live for this call.
        unsafe { scalar_text_arg(&self.values, index, invalid_type_message) }
    }
}

unsafe fn scalar_args(
    argc: c_int,
    argv: *mut *mut ffi::sqlite3_value,
) -> LithographResult<ScalarArgs> {
    let length = usize::try_from(argc)
        .map_err(|_| LithographError::internal("SQLite supplied a negative scalar argc"))?;
    if length == 0 {
        return Ok(ScalarArgs { values: Vec::new() });
    }
    if argv.is_null() {
        return Err(LithographError::internal(
            "SQLite supplied NULL scalar argv with non-zero argc",
        ));
    }
    // SAFETY: SQLite supplies `argc` valid sqlite3_value* entries for the
    // callback duration.
    let values = unsafe { std::slice::from_raw_parts(argv, length) }.to_vec();
    Ok(ScalarArgs { values })
}

unsafe fn scalar_text_arg(
    args: &[*mut ffi::sqlite3_value],
    index: usize,
    invalid_type_message: &str,
) -> LithographResult<String> {
    let Some(&value) = args.get(index) else {
        return Err(LithographError::internal(
            "SQLite scalar callback is missing a registered argument",
        ));
    };
    if value.is_null() {
        return Err(LithographError::internal(
            "SQLite supplied a NULL sqlite3_value pointer",
        ));
    }
    // SAFETY: `value` belongs to the active SQLite callback.
    if unsafe { ffi::sqlite3_value_type(value) } != ffi::SQLITE_TEXT {
        return Err(LithographError::invalid_argument(invalid_type_message));
    }
    // SAFETY: SQLite converts the value to UTF-8 and owns the pointer for the
    // callback duration.
    let text = unsafe { ffi::sqlite3_value_text(value) };
    if text.is_null() {
        return Err(LithographError::new(
            ErrorCategory::Resource,
            "SQLite could not materialize scalar TEXT input",
            ffi::SQLITE_NOMEM,
        ));
    }
    // SAFETY: called after sqlite3_value_text(), so this length describes the
    // UTF-8 representation above.
    let length = unsafe { ffi::sqlite3_value_bytes(value) };
    let length = usize::try_from(length)
        .map_err(|_| LithographError::internal("SQLite returned a negative TEXT byte length"))?;
    // SAFETY: the UTF-8 pointer is readable for `length` bytes until callback
    // return; copy it into owned Rust memory now.
    let bytes = unsafe { std::slice::from_raw_parts(text, length) };
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| LithographError::invalid_argument("TEXT input must contain valid UTF-8"))
}
