//! OpenAI-compatible embedding provider for Lithograph.
//!
//! This crate is a standalone SQLite loadable extension. It registers the
//! `openai-compatible` `EmbeddingProviderV1` in SQLite connection client data
//! and does not depend on Lithograph Core.

#![allow(
    unsafe_code,
    reason = "SQLite loadable-extension and embedding-provider ABI boundaries require raw FFI"
)]
#![cfg_attr(
    not(test),
    deny(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]

mod config;
mod http;

use std::ffi::{CStr, c_char, c_int, c_void};
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::OnceLock;

use config::{MAX_BATCH_SIZE, ProviderConfig};
use http::{Client, Failure, FailureKind};
use lithograph_embedding_provider::{
    EMBEDDING_COORDINATE_FLOAT32_V1, EMBEDDING_PROVIDER_ABI_VERSION_V1, EmbeddingBatchV1,
    EmbeddingCancelCallbackV1, EmbeddingErrorV1, EmbeddingProviderV1, EmbeddingStatusV1,
    EmbeddingTextV1,
};
use rusqlite::{Connection, Result as SqliteResult, ffi};

const SQLITE_MIN_VERSION_NUMBER: c_int = 3_045_000;
const SQLITE_API_GET_CLIENTDATA_SLOT: usize = 268;
const SQLITE_API_SET_CLIENTDATA_SLOT: usize = 269;
const PROVIDER_KEY: &CStr = c"lithograph.embedding.v1/openai-compatible";
const PROVIDER_SEMANTIC_IDENTITY: &[u8] = b"lithograph-openai-compatible/v1";
const MAX_DIMENSIONS: usize = 4_096;

type SqliteGetClientdata = unsafe extern "C" fn(*mut ffi::sqlite3, *const c_char) -> *mut c_void;
type SqliteSetClientdata = unsafe extern "C" fn(
    *mut ffi::sqlite3,
    *const c_char,
    *mut c_void,
    Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int;

static SQLITE_GET_CLIENTDATA: OnceLock<SqliteGetClientdata> = OnceLock::new();
static SQLITE_SET_CLIENTDATA: OnceLock<SqliteSetClientdata> = OnceLock::new();

struct ProviderRuntime {
    client: Client,
}

/// Generic SQLite extension entry point.
///
/// # Safety
///
/// SQLite supplies all pointers according to the loadable-extension ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_extension_init(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    // SAFETY: forwarded unchanged from SQLite to the shared implementation.
    unsafe { extension_entry(db, pz_err_msg, p_api) }
}

/// Filename-derived SQLite entry point for `lithograph_openai_compatible`.
///
/// # Safety
///
/// SQLite supplies all pointers according to the loadable-extension ABI.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_lithographopenaicompatible_init(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    // SAFETY: forwarded unchanged from SQLite to the shared implementation.
    unsafe { extension_entry(db, pz_err_msg, p_api) }
}

unsafe fn extension_entry(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    match catch_unwind(AssertUnwindSafe(|| {
        if !host_sqlite_version_supported(p_api) || !capture_clientdata_apis(p_api) {
            return ffi::SQLITE_ERROR;
        }
        // SAFETY: SQLite owns the live connection and API table for this init
        // callback. rusqlite installs its extension thunk before the closure.
        unsafe { Connection::extension_init2(db, pz_err_msg, p_api, extension_init) }
    })) {
        Ok(code) => code,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

fn extension_init(connection: Connection) -> SqliteResult<bool> {
    ensure_dependency_logging_disabled().map_err(sqlite_init_error)?;
    // SAFETY: connection remains live for registration and SQLite owns client
    // data after successful set_clientdata.
    let db = unsafe { connection.handle() };
    register_provider(db).map_err(sqlite_init_error)?;
    Ok(false)
}

fn ensure_dependency_logging_disabled() -> Result<(), String> {
    if log::STATIC_MAX_LEVEL == log::LevelFilter::Off {
        return Ok(());
    }
    Err("openai-compatible provider requires compile-time-disabled dependency logging".to_owned())
}

fn sqlite_init_error(message: String) -> rusqlite::Error {
    rusqlite::Error::SqliteFailure(ffi::Error::new(ffi::SQLITE_ERROR), Some(message))
}

fn register_provider(db: *mut ffi::sqlite3) -> Result<(), String> {
    if !host_get_clientdata(db, PROVIDER_KEY).is_null() {
        return Err("openai-compatible embedding provider is already registered".to_owned());
    }

    let mut runtime = Box::new(ProviderRuntime {
        client: Client::new(),
    });
    let context = (&mut *runtime as *mut ProviderRuntime).cast::<c_void>();
    let provider = Box::new(EmbeddingProviderV1 {
        abi_version: EMBEDDING_PROVIDER_ABI_VERSION_V1,
        struct_size: std::mem::size_of::<EmbeddingProviderV1>(),
        context,
        semantic_identity: PROVIDER_SEMANTIC_IDENTITY.as_ptr(),
        semantic_identity_len: PROVIDER_SEMANTIC_IDENTITY.len(),
        validate: Some(validate_callback),
        embed_batch: Some(embed_batch_callback),
        free_batch: Some(free_batch_callback),
        free_error: Some(free_error_callback),
    });
    let runtime = Box::into_raw(runtime);
    debug_assert_eq!(context, runtime.cast::<c_void>());
    let provider = Box::into_raw(provider);

    let code = host_set_clientdata(db, PROVIDER_KEY, provider.cast(), Some(destroy_provider));
    if code == ffi::SQLITE_OK {
        return Ok(());
    }
    if code != ffi::SQLITE_NOMEM {
        // SQLite documents automatic destructor invocation on SQLITE_NOMEM.
        // Other result codes are unexpected, so ownership remains here.
        // SAFETY: provider/runtime were allocated above and are not registered.
        unsafe { destroy_provider(provider.cast()) };
    }
    Err(format!(
        "failed to register openai-compatible embedding provider (SQLite code {code})"
    ))
}

unsafe extern "C" fn destroy_provider(data: *mut c_void) {
    if data.is_null() {
        return;
    }
    // SAFETY: SQLite calls this for the exact provider allocation registered
    // by register_provider.
    let provider = unsafe { Box::from_raw(data.cast::<EmbeddingProviderV1>()) };
    if !provider.context.is_null() {
        // SAFETY: context was allocated as ProviderRuntime and transferred with
        // the provider registration.
        drop(unsafe { Box::from_raw(provider.context.cast::<ProviderRuntime>()) });
    }
}

unsafe extern "C" fn validate_callback(
    context: *mut c_void,
    config_json: *const u8,
    config_len: usize,
    dimensions: usize,
    coordinate_type: u32,
    error: *mut EmbeddingErrorV1,
) -> c_int {
    let operation = || {
        // SAFETY: this unsafe ABI callback receives the provider context and
        // config buffer from the caller for the callback duration.
        unsafe {
            validate_request(
                context,
                config_json,
                config_len,
                dimensions,
                coordinate_type,
            )
        }?;
        Ok(())
    };
    // SAFETY: SQLite/Lithograph supplies the writable ABI error pointer for
    // the duration of this callback.
    unsafe { ffi_boundary(error, operation) }
}

unsafe extern "C" fn embed_batch_callback(
    context: *mut c_void,
    config_json: *const u8,
    config_len: usize,
    texts: *const EmbeddingTextV1,
    text_count: usize,
    dimensions: usize,
    coordinate_type: u32,
    is_cancelled: EmbeddingCancelCallbackV1,
    cancel_user_data: *mut c_void,
    result: *mut EmbeddingBatchV1,
    error: *mut EmbeddingErrorV1,
) -> c_int {
    // #lizard forgives(parameter_count)
    let operation = || {
        if result.is_null() {
            return Err(Failure::internal("result pointer must not be null"));
        }
        // SAFETY: this unsafe ABI callback receives the provider context and
        // config buffer from the caller for the callback duration.
        let config = unsafe {
            validate_request(
                context,
                config_json,
                config_len,
                dimensions,
                coordinate_type,
            )
        }?;
        // SAFETY: the ABI caller owns the text descriptor array and backing
        // bytes for the callback duration.
        let texts = unsafe { owned_texts(texts, text_count) }?;
        // SAFETY: validate_request confirmed this provider-owned context.
        let runtime = unsafe { &*context.cast::<ProviderRuntime>() };
        let values = runtime.client.embed(&config, &texts, dimensions, || {
            // SAFETY: callback/user data are caller-owned for embedBatch.
            unsafe { cancelled(is_cancelled, cancel_user_data) }
        })?;
        let mut values = values.into_boxed_slice();
        let value_count = values.len();
        let values_ptr = values.as_mut_ptr();
        std::mem::forget(values);
        // SAFETY: caller supplied a writable result pointer and now owns the
        // provider allocation until free_batch is called.
        unsafe {
            *result = EmbeddingBatchV1 {
                values: values_ptr,
                value_count,
                embedding_count: text_count,
                dimensions,
            };
        }
        Ok(())
    };
    // SAFETY: SQLite/Lithograph supplies the writable ABI error pointer for
    // the duration of this callback.
    unsafe { ffi_boundary(error, operation) }
}

unsafe fn validate_request(
    context: *mut c_void,
    config_json: *const u8,
    config_len: usize,
    dimensions: usize,
    coordinate_type: u32,
) -> Result<ProviderConfig, Failure> {
    if context.is_null() {
        return Err(Failure::internal("provider context is null"));
    }
    if coordinate_type != EMBEDDING_COORDINATE_FLOAT32_V1 {
        return Err(Failure::invalid(
            "openai-compatible provider only supports FLOAT32 output",
        ));
    }
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        return Err(Failure::invalid(
            "dimensions must be an integer between 1 and 4096",
        ));
    }
    // SAFETY: validate/embed callbacks receive this buffer from the ABI caller
    // for the duration of the call.
    let config_bytes = unsafe { bytes_from_raw(config_json, config_len, "providerConfig")? };
    ProviderConfig::parse(config_bytes).map_err(Failure::invalid)
}

unsafe fn owned_texts(texts: *const EmbeddingTextV1, count: usize) -> Result<Vec<String>, Failure> {
    if count == 0 {
        return Err(Failure::invalid(
            "embedding batch must contain at least one text",
        ));
    }
    if count > MAX_BATCH_SIZE {
        return Err(Failure::resource(format!(
            "embedding batch exceeds the {MAX_BATCH_SIZE}-item provider limit"
        )));
    }
    if texts.is_null() {
        return Err(Failure::internal("text array pointer is null"));
    }
    // SAFETY: caller promises an array of count ABI text entries for the call.
    let texts = unsafe { std::slice::from_raw_parts(texts, count) };
    texts
        .iter()
        .map(|text| {
            // SAFETY: each descriptor belongs to the caller-owned array and
            // its backing bytes remain live for the callback duration.
            let bytes = unsafe { bytes_from_raw(text.data, text.len, "embedding text")? };
            let text = std::str::from_utf8(bytes)
                .map_err(|_| Failure::invalid("embedding text must be valid UTF-8"))?;
            Ok(text.to_owned())
        })
        .collect()
}

unsafe fn bytes_from_raw<'a>(data: *const u8, len: usize, name: &str) -> Result<&'a [u8], Failure> {
    if len == 0 {
        return Ok(&[]);
    }
    if len > isize::MAX as usize {
        return Err(Failure::resource(format!(
            "{name} length exceeds addressable memory"
        )));
    }
    if data.is_null() {
        return Err(Failure::internal(format!("{name} pointer is null")));
    }
    // SAFETY: caller promises a readable buffer of len bytes for the callback.
    Ok(unsafe { std::slice::from_raw_parts(data, len) })
}

unsafe fn cancelled(callback: EmbeddingCancelCallbackV1, user_data: *mut c_void) -> bool {
    callback.is_some_and(|callback| {
        // SAFETY: callback and user_data are caller-owned for embed_batch.
        unsafe { callback(user_data) != 0 }
    })
}

unsafe fn ffi_boundary(
    error: *mut EmbeddingErrorV1,
    operation: impl FnOnce() -> Result<(), Failure>,
) -> c_int {
    // SAFETY: caller provides the writable ABI error slot for this callback.
    unsafe { clear_error(error) };
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(Ok(())) => EmbeddingStatusV1::Ok.code(),
        Ok(Err(failure)) => {
            let status = status_for_failure(failure.kind);
            // SAFETY: same ABI error slot validated by the callback contract.
            unsafe { write_error(error, status, &failure.message) };
            status.code()
        }
        Err(_) => {
            let status = EmbeddingStatusV1::InternalError;
            // SAFETY: same ABI error slot validated by the callback contract.
            unsafe { write_error(error, status, "embedding provider panicked") };
            status.code()
        }
    }
}

fn status_for_failure(kind: FailureKind) -> EmbeddingStatusV1 {
    match kind {
        FailureKind::InvalidConfig => EmbeddingStatusV1::InvalidConfig,
        FailureKind::Io => EmbeddingStatusV1::IoError,
        FailureKind::Resource => EmbeddingStatusV1::ResourceError,
        FailureKind::Cancelled => EmbeddingStatusV1::Cancelled,
        FailureKind::Internal => EmbeddingStatusV1::InternalError,
    }
}

unsafe fn write_error(error: *mut EmbeddingErrorV1, status: EmbeddingStatusV1, message: &str) {
    if error.is_null() {
        return;
    }
    let mut bytes = message.as_bytes().to_vec().into_boxed_slice();
    let message_len = bytes.len();
    let message = bytes.as_mut_ptr();
    std::mem::forget(bytes);
    // SAFETY: caller supplied a writable error struct for this callback.
    unsafe {
        *error = EmbeddingErrorV1 {
            status: status.code(),
            message,
            message_len,
        };
    }
}

unsafe fn clear_error(error: *mut EmbeddingErrorV1) {
    if error.is_null() {
        return;
    }
    // SAFETY: caller supplied a writable error struct for this callback.
    unsafe { *error = EmbeddingErrorV1::empty() };
}

unsafe extern "C" fn free_batch_callback(_context: *mut c_void, result: *mut EmbeddingBatchV1) {
    if result.is_null() {
        return;
    }
    // SAFETY: caller passes a result previously filled by this provider.
    let result_ref = unsafe { &mut *result };
    if !result_ref.values.is_null() && result_ref.value_count > 0 {
        let slice = ptr::slice_from_raw_parts_mut(result_ref.values, result_ref.value_count);
        // SAFETY: values came from a Box<[f32]> leaked by embed_batch_callback.
        drop(unsafe { Box::from_raw(slice) });
    }
    *result_ref = EmbeddingBatchV1::empty();
}

unsafe extern "C" fn free_error_callback(_context: *mut c_void, error: *mut EmbeddingErrorV1) {
    if error.is_null() {
        return;
    }
    // SAFETY: caller passes an error previously filled by this provider.
    let error_ref = unsafe { &mut *error };
    if !error_ref.message.is_null() && error_ref.message_len > 0 {
        let slice = ptr::slice_from_raw_parts_mut(error_ref.message, error_ref.message_len);
        // SAFETY: message came from a Box<[u8]> leaked by write_error.
        drop(unsafe { Box::from_raw(slice) });
    }
    *error_ref = EmbeddingErrorV1::empty();
}

fn host_sqlite_version_supported(p_api: *mut ffi::sqlite3_api_routines) -> bool {
    if p_api.is_null() {
        return false;
    }
    // SAFETY: libversion_number is in the original extension API prefix.
    let Some(function) = (unsafe { (*p_api).libversion_number }) else {
        return false;
    };
    // SAFETY: function pointer comes from the live host API table.
    unsafe { function() >= SQLITE_MIN_VERSION_NUMBER }
}

fn capture_clientdata_apis(p_api: *mut ffi::sqlite3_api_routines) -> bool {
    if SQLITE_GET_CLIENTDATA.get().is_some() && SQLITE_SET_CLIENTDATA.get().is_some() {
        return true;
    }
    if p_api.is_null() {
        return false;
    }
    let slots = p_api.cast::<*const c_void>();
    // SAFETY: SQLite >= 3.45 includes append-only slots 268/269.
    let get_slot = unsafe { slots.add(SQLITE_API_GET_CLIENTDATA_SLOT) };
    // SAFETY: the slot points inside the live host-owned API table.
    let get = unsafe { get_slot.read() };
    // SAFETY: same supported host API table as above.
    let set_slot = unsafe { slots.add(SQLITE_API_SET_CLIENTDATA_SLOT) };
    // SAFETY: the slot points inside the live host-owned API table.
    let set = unsafe { set_slot.read() };
    if get.is_null() || set.is_null() {
        return false;
    }
    // SAFETY: SQLite documents these exact signatures for slots 268/269.
    let get = unsafe { std::mem::transmute::<*const c_void, SqliteGetClientdata>(get) };
    // SAFETY: same API-slot contract as above.
    let set = unsafe { std::mem::transmute::<*const c_void, SqliteSetClientdata>(set) };
    let _ = SQLITE_GET_CLIENTDATA.set(get);
    let _ = SQLITE_SET_CLIENTDATA.set(set);
    true
}

fn host_get_clientdata(db: *mut ffi::sqlite3, key: &CStr) -> *mut c_void {
    SQLITE_GET_CLIENTDATA
        .get()
        .map_or(ptr::null_mut(), |function| {
            // SAFETY: function comes from host table, db is live, key is terminated.
            unsafe { function(db, key.as_ptr()) }
        })
}

fn host_set_clientdata(
    db: *mut ffi::sqlite3,
    key: &CStr,
    data: *mut c_void,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    SQLITE_SET_CLIENTDATA
        .get()
        .map_or(ffi::SQLITE_MISUSE, |function| {
            // SAFETY: function comes from host table; SQLite takes ownership on
            // success and invokes destructor on SQLITE_NOMEM.
            unsafe { function(db, key.as_ptr(), data, destructor) }
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn provider_abi_constants_match_public_contract() {
        assert_eq!(EMBEDDING_PROVIDER_ABI_VERSION_V1, 1);
        assert_eq!(EMBEDDING_COORDINATE_FLOAT32_V1, 1);
        assert!(std::mem::size_of::<EmbeddingProviderV1>() > 0);
    }

    #[test]
    fn dependency_logging_is_compile_time_disabled() {
        assert_eq!(log::STATIC_MAX_LEVEL, log::LevelFilter::Off);
        ensure_dependency_logging_disabled().expect("dependency logging must stay disabled");
    }

    #[test]
    fn raw_input_limits_fail_before_pointer_dereference() {
        let oversized_count = MAX_BATCH_SIZE + 1;
        // SAFETY: the oversized count must be rejected before the null pointer
        // could be dereferenced.
        let error =
            unsafe { owned_texts(ptr::null(), oversized_count) }.expect_err("oversized batch");
        assert_eq!(error.kind, FailureKind::Resource);

        let oversized_len = (isize::MAX as usize).saturating_add(1);
        // SAFETY: the oversized length must be rejected before the null pointer
        // could be dereferenced.
        let error = unsafe { bytes_from_raw(ptr::null(), oversized_len, "test") }
            .expect_err("oversized raw buffer");
        assert_eq!(error.kind, FailureKind::Resource);
    }
}
