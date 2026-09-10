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
    _user_data: *mut c_void,
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
            )
        }
    };
    // SAFETY: the public ABI contract guarantees all non-NULL pointers are
    // valid for this call, including writable `error_json` out-parameter storage.
    unsafe { native_boundary(error_json, operation) }
}

unsafe fn native_boundary(
    error_json: *mut *mut c_char,
    operation: impl FnOnce() -> LithographResult<()>,
) -> c_int {
    // SAFETY: callers guarantee a non-NULL pointer references writable ABI
    // out-parameter storage.
    unsafe { clear_error_out(error_json) };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => ffi::SQLITE_OK,
        Ok(Err(error)) => {
            // SAFETY: the same caller contract covers the error out-pointer.
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
) -> LithographResult<()> {
    if db.is_null() {
        return Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            "db must not be NULL",
            ffi::SQLITE_MISUSE,
        ));
    }
    if callback.is_none() {
        return Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            "callback must not be NULL",
            ffi::SQLITE_MISUSE,
        ));
    }
    require_native_connection_registered(db)?;

    // SAFETY: each pointer/length pair comes directly from the public ABI call
    // and remains readable for this invocation.
    let query = unsafe { input_utf8(query.pointer, query.length, "query")? };
    // SAFETY: same ABI lifetime contract as `query` above.
    let params = unsafe { input_utf8(params_json.pointer, params_json.length, "params_json")? };
    // SAFETY: same ABI lifetime contract as `query` above.
    let options = unsafe { input_utf8(options_json.pointer, options_json.length, "options_json")? };
    if query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty"));
    }
    cypher::decode_parameters_text(&params)
        .map_err(|error| LithographError::invalid_argument(error.message))?;
    validate_json_object(&options, "options")?;
    // SAFETY: registration proves `db` is a live SQLite connection containing
    // this extension; rusqlite borrows the handle without taking ownership.
    let connection = unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "invalid SQLite connection"))?;
    require_initialized(&connection)?;
    validate_cypher(&query)?;

    Err(LithographError::execution_unavailable())
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
    unsafe { native_boundary(error_json, operation) }
}

unsafe fn native_validate_impl(
    db: *mut ffi::sqlite3,
    query: *const c_char,
    query_len: usize,
) -> LithographResult<()> {
    if db.is_null() {
        return Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            "db must not be NULL",
            ffi::SQLITE_MISUSE,
        ));
    }
    require_native_connection_registered(db)?;
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
    require_initialized(&connection)?;
    validate_cypher(&query)?;
    Ok(())
}

fn require_native_connection_registered(db: *mut ffi::sqlite3) -> LithographResult<()> {
    let registered = registered_connections()
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .contains_key(&(db as usize));
    if !registered {
        return Err(LithographError::new(
            ErrorCategory::InvalidArgument,
            "target SQLite connection has not loaded the Lithograph extension",
            ffi::SQLITE_MISUSE,
        ));
    }
    Ok(())
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
