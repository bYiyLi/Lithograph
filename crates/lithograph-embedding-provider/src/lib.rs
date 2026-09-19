//! Public C-compatible ABI types for Lithograph embedding providers.
//!
//! Provider implementations are ordinary SQLite extensions. They share only
//! these ABI types with Lithograph and do not depend on Lithograph Core.

#![allow(
    unsafe_code,
    reason = "this crate owns the SQLite client-data and provider C ABI boundary"
)]

use std::ffi::{CString, c_char, c_int, c_void};
use std::marker::PhantomData;
use std::ptr;
use std::slice;
use std::sync::OnceLock;

use rusqlite::{Connection, ffi};

pub const EMBEDDING_PROVIDER_ABI_VERSION_V1: u32 = 1;
pub const EMBEDDING_COORDINATE_FLOAT32_V1: u32 = 1;

#[repr(i32)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EmbeddingStatusV1 {
    Ok = 0,
    InvalidConfig = 1,
    IoError = 2,
    ResourceError = 3,
    Cancelled = 4,
    InternalError = 5,
}

impl EmbeddingStatusV1 {
    #[must_use]
    pub const fn code(self) -> c_int {
        self as c_int
    }
}

#[repr(C)]
#[derive(Debug, Clone, Copy)]
pub struct EmbeddingTextV1 {
    pub data: *const u8,
    pub len: usize,
}

#[repr(C)]
#[derive(Debug)]
pub struct EmbeddingErrorV1 {
    pub status: c_int,
    pub message: *mut u8,
    pub message_len: usize,
}

impl EmbeddingErrorV1 {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            status: EmbeddingStatusV1::Ok as c_int,
            message: ptr::null_mut(),
            message_len: 0,
        }
    }
}

#[repr(C)]
#[derive(Debug)]
pub struct EmbeddingBatchV1 {
    pub values: *mut f32,
    pub value_count: usize,
    pub embedding_count: usize,
    pub dimensions: usize,
}

impl EmbeddingBatchV1 {
    #[must_use]
    pub const fn empty() -> Self {
        Self {
            values: ptr::null_mut(),
            value_count: 0,
            embedding_count: 0,
            dimensions: 0,
        }
    }
}

pub type EmbeddingCancelCallbackV1 = Option<unsafe extern "C" fn(user_data: *mut c_void) -> c_int>;

pub type EmbeddingValidateV1 = Option<
    unsafe extern "C" fn(
        context: *mut c_void,
        config_json: *const u8,
        config_len: usize,
        dimensions: usize,
        coordinate_type: u32,
        error: *mut EmbeddingErrorV1,
    ) -> c_int,
>;

pub type EmbeddingEmbedBatchV1 = Option<
    unsafe extern "C" fn(
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
    ) -> c_int,
>;

pub type EmbeddingFreeBatchV1 =
    Option<unsafe extern "C" fn(context: *mut c_void, result: *mut EmbeddingBatchV1)>;

pub type EmbeddingFreeErrorV1 =
    Option<unsafe extern "C" fn(context: *mut c_void, error: *mut EmbeddingErrorV1)>;

#[repr(C)]
pub struct EmbeddingProviderV1 {
    pub abi_version: u32,
    pub struct_size: usize,
    pub context: *mut c_void,
    pub semantic_identity: *const u8,
    pub semantic_identity_len: usize,
    pub validate: EmbeddingValidateV1,
    pub embed_batch: EmbeddingEmbedBatchV1,
    pub free_batch: EmbeddingFreeBatchV1,
    pub free_error: EmbeddingFreeErrorV1,
}

#[repr(C)]
#[derive(Clone, Copy)]
struct EmbeddingProviderHeaderV1 {
    abi_version: u32,
    struct_size: usize,
}

const PROVIDER_KEY_PREFIX: &str = "lithograph.embedding.v1/";
const MAX_SEMANTIC_IDENTITY_BYTES: usize = 256;
pub type SqliteGetClientdataV1 =
    unsafe extern "C" fn(*mut ffi::sqlite3, *const c_char) -> *mut c_void;
static SQLITE_GET_CLIENTDATA: OnceLock<SqliteGetClientdataV1> = OnceLock::new();

/// Installs the host SQLite client-data getter used by the safe provider host
/// wrapper.
///
/// # Safety
///
/// The function pointer must be SQLite's process-lifetime
/// sqlite3_get_clientdata implementation for the same host API used by
/// connections passed to RegisteredEmbeddingProvider::lookup.
pub unsafe fn install_sqlite_get_clientdata(function: SqliteGetClientdataV1) {
    let _ = SQLITE_GET_CLIENTDATA.set(function);
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProviderErrorKind {
    Missing,
    InvalidRegistration,
    InvalidConfig,
    Io,
    Resource,
    Cancelled,
    Internal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProviderError {
    pub kind: ProviderErrorKind,
    pub message: String,
}

impl ProviderError {
    fn new(kind: ProviderErrorKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

pub struct RegisteredEmbeddingProvider<'connection> {
    provider: &'connection EmbeddingProviderV1,
    semantic_identity: &'connection str,
    _connection: PhantomData<&'connection Connection>,
}

impl<'connection> RegisteredEmbeddingProvider<'connection> {
    pub fn lookup(connection: &'connection Connection, name: &str) -> Result<Self, ProviderError> {
        let key = provider_key(name)?;
        let get_clientdata = SQLITE_GET_CLIENTDATA.get().ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRegistration,
                "embedding provider host API is not initialized",
            )
        })?;
        // SAFETY: rusqlite owns a live sqlite3 connection for the entire
        // returned provider lifetime.
        let db = unsafe { connection.handle() };
        // SAFETY: db is the live connection handle above, the key is a valid C
        // string, and the getter was captured from the matching SQLite host API.
        let data = unsafe { get_clientdata(db, key.as_ptr()) };
        if data.is_null() {
            return Err(ProviderError::new(
                ProviderErrorKind::Missing,
                format!("embedding provider {name:?} is not registered on this SQLite connection"),
            ));
        }
        // SAFETY: the client-data namespace is the public embedding-provider
        // ABI namespace. The helper reads only the fixed header prefix before
        // it trusts struct_size enough to form a full EmbeddingProviderV1
        // reference.
        let provider = unsafe { provider_from_clientdata(data, name)? };
        // SAFETY: validation proved a non-null identity pointer and bounded
        // length owned by the provider for its connection registration.
        let identity = unsafe {
            slice::from_raw_parts(provider.semantic_identity, provider.semantic_identity_len)
        };
        let semantic_identity = std::str::from_utf8(identity).map_err(|_| {
            ProviderError::new(
                ProviderErrorKind::InvalidRegistration,
                format!("embedding provider {name:?} semantic identity is not valid UTF-8"),
            )
        })?;
        Ok(Self {
            provider,
            semantic_identity,
            _connection: PhantomData,
        })
    }

    #[must_use]
    pub fn semantic_identity(&self) -> &str {
        self.semantic_identity
    }

    pub fn validate(&self, config_json: &[u8], dimensions: usize) -> Result<(), ProviderError> {
        let callback = self.provider.validate.ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRegistration,
                "embedding provider validate callback is missing",
            )
        })?;
        let mut error = EmbeddingErrorV1::empty();
        // SAFETY: callback and context were validated from the live
        // connection registration; config bytes live for the call.
        let status = unsafe {
            callback(
                self.provider.context,
                config_json.as_ptr(),
                config_json.len(),
                dimensions,
                EMBEDDING_COORDINATE_FLOAT32_V1,
                &mut error,
            )
        };
        self.finish_status(status, &mut error)
    }

    pub fn embed_batch(
        &self,
        config_json: &[u8],
        texts: &[&str],
        dimensions: usize,
        is_cancelled: &dyn Fn() -> bool,
    ) -> Result<Vec<f32>, ProviderError> {
        if is_cancelled() {
            return Err(cancelled_error());
        }
        let callback = self.provider.embed_batch.ok_or_else(|| {
            ProviderError::new(
                ProviderErrorKind::InvalidRegistration,
                "embedding provider embed_batch callback is missing",
            )
        })?;
        let abi_texts = texts
            .iter()
            .map(|text| EmbeddingTextV1 {
                data: text.as_ptr(),
                len: text.len(),
            })
            .collect::<Vec<_>>();
        let mut batch = EmbeddingBatchV1::empty();
        let mut error = EmbeddingErrorV1::empty();
        let cancel = CancelContext { is_cancelled };
        // SAFETY: all pointers reference call-local buffers for the callback
        // duration; provider registration and callbacks were validated.
        let status = unsafe {
            callback(
                self.provider.context,
                config_json.as_ptr(),
                config_json.len(),
                abi_texts.as_ptr(),
                abi_texts.len(),
                dimensions,
                EMBEDDING_COORDINATE_FLOAT32_V1,
                Some(cancel_callback),
                ptr::from_ref(&cancel).cast_mut().cast(),
                &mut batch,
                &mut error,
            )
        };
        if let Err(failure) = self.finish_status(status, &mut error) {
            self.free_batch(&mut batch);
            return Err(failure);
        }
        if is_cancelled() {
            self.free_batch(&mut batch);
            return Err(cancelled_error());
        }
        let result = validate_batch(&batch, texts.len(), dimensions).map(|()| {
            // SAFETY: validate_batch proved a non-null buffer containing the
            // exact expected number of FLOAT32 values.
            unsafe { slice::from_raw_parts(batch.values, batch.value_count) }.to_vec()
        });
        self.free_batch(&mut batch);
        result
    }

    fn finish_status(
        &self,
        status: c_int,
        error: &mut EmbeddingErrorV1,
    ) -> Result<(), ProviderError> {
        if status == EmbeddingStatusV1::Ok.code() {
            self.free_error(error);
            return Ok(());
        }
        let message = error_message(error)
            .unwrap_or_else(|| format!("embedding provider returned status {status}"));
        let kind = error_kind(status);
        self.free_error(error);
        Err(ProviderError::new(kind, message))
    }

    fn free_batch(&self, batch: &mut EmbeddingBatchV1) {
        if let Some(callback) = self.provider.free_batch {
            // SAFETY: batch is either provider-owned output or the ABI empty
            // value and this is the provider's matching release callback.
            unsafe { callback(self.provider.context, batch) };
        }
    }

    fn free_error(&self, error: &mut EmbeddingErrorV1) {
        if let Some(callback) = self.provider.free_error {
            // SAFETY: error is provider-owned output or the ABI empty value
            // and this is the provider's matching release callback.
            unsafe { callback(self.provider.context, error) };
        }
    }
}

fn cancelled_error() -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::Cancelled,
        "embedding provider call was cancelled",
    )
}

struct CancelContext<'a> {
    is_cancelled: &'a dyn Fn() -> bool,
}

unsafe extern "C" fn cancel_callback(user_data: *mut c_void) -> c_int {
    if user_data.is_null() {
        return 0;
    }
    // SAFETY: embed_batch passes a live CancelContext for the duration of the
    // provider callback.
    let context = unsafe { &*user_data.cast::<CancelContext<'_>>() };
    c_int::from((context.is_cancelled)())
}

fn provider_key(name: &str) -> Result<CString, ProviderError> {
    if name.is_empty() {
        return Err(ProviderError::new(
            ProviderErrorKind::InvalidConfig,
            "embedding provider name must not be empty",
        ));
    }
    CString::new(format!("{PROVIDER_KEY_PREFIX}{name}")).map_err(|_| {
        ProviderError::new(
            ProviderErrorKind::InvalidConfig,
            "embedding provider name must not contain NUL",
        )
    })
}

/// Resolves one client-data pointer without forming a full V1 reference until
/// the fixed ABI header proves the allocation is large enough.
///
/// # Safety
///
/// data must point to a live provider registration whose allocation contains
/// at least the common abi_version / struct_size header. If that header
/// advertises a V1-sized allocation, the allocation must actually contain that
/// many readable bytes for the registration lifetime.
unsafe fn provider_from_clientdata<'a>(
    data: *mut c_void,
    name: &str,
) -> Result<&'a EmbeddingProviderV1, ProviderError> {
    // SAFETY: the public provider ABI freezes these two fields as its minimum
    // common prefix; no later V1 field is touched before the size check.
    let header = unsafe { &*data.cast::<EmbeddingProviderHeaderV1>() };
    validate_registration_header(header, name)?;
    // SAFETY: header validation established a V1-sized allocation according
    // to the provider ABI contract.
    let provider = unsafe { &*data.cast::<EmbeddingProviderV1>() };
    validate_registration_body(provider, name)?;
    Ok(provider)
}

#[cfg(test)]
fn validate_registration(provider: &EmbeddingProviderV1, name: &str) -> Result<(), ProviderError> {
    validate_registration_header(
        &EmbeddingProviderHeaderV1 {
            abi_version: provider.abi_version,
            struct_size: provider.struct_size,
        },
        name,
    )?;
    validate_registration_body(provider, name)
}

fn validate_registration_header(
    header: &EmbeddingProviderHeaderV1,
    name: &str,
) -> Result<(), ProviderError> {
    if header.abi_version != EMBEDDING_PROVIDER_ABI_VERSION_V1 {
        return Err(invalid_registration(
            name,
            format!("unsupported ABI version {}", header.abi_version),
        ));
    }
    if header.struct_size < std::mem::size_of::<EmbeddingProviderV1>() {
        return Err(invalid_registration(name, "ABI struct is too small"));
    }
    Ok(())
}

fn validate_registration_body(
    provider: &EmbeddingProviderV1,
    name: &str,
) -> Result<(), ProviderError> {
    if provider.semantic_identity.is_null()
        || provider.semantic_identity_len == 0
        || provider.semantic_identity_len > MAX_SEMANTIC_IDENTITY_BYTES
    {
        return Err(invalid_registration(
            name,
            "semantic identity is missing or exceeds the ABI limit",
        ));
    }
    if provider.validate.is_none()
        || provider.embed_batch.is_none()
        || provider.free_batch.is_none()
        || provider.free_error.is_none()
    {
        return Err(invalid_registration(
            name,
            "required ABI callback is missing",
        ));
    }
    Ok(())
}

fn invalid_registration(name: &str, detail: impl std::fmt::Display) -> ProviderError {
    ProviderError::new(
        ProviderErrorKind::InvalidRegistration,
        format!("embedding provider {name:?} has invalid registration: {detail}"),
    )
}

fn error_message(error: &EmbeddingErrorV1) -> Option<String> {
    if error.message.is_null() || error.message_len == 0 {
        return None;
    }
    // SAFETY: a non-empty provider error owns this message until free_error.
    let bytes = unsafe { slice::from_raw_parts(error.message, error.message_len) };
    Some(String::from_utf8_lossy(bytes).into_owned())
}

fn error_kind(status: c_int) -> ProviderErrorKind {
    match status {
        value if value == EmbeddingStatusV1::InvalidConfig.code() => {
            ProviderErrorKind::InvalidConfig
        }
        value if value == EmbeddingStatusV1::IoError.code() => ProviderErrorKind::Io,
        value if value == EmbeddingStatusV1::ResourceError.code() => ProviderErrorKind::Resource,
        value if value == EmbeddingStatusV1::Cancelled.code() => ProviderErrorKind::Cancelled,
        _ => ProviderErrorKind::Internal,
    }
}

fn validate_batch(
    batch: &EmbeddingBatchV1,
    text_count: usize,
    dimensions: usize,
) -> Result<(), ProviderError> {
    let expected = text_count.checked_mul(dimensions).ok_or_else(|| {
        ProviderError::new(
            ProviderErrorKind::Resource,
            "embedding provider output size overflow",
        )
    })?;
    if batch.embedding_count != text_count
        || batch.dimensions != dimensions
        || batch.value_count != expected
        || (expected > 0 && batch.values.is_null())
    {
        return Err(ProviderError::new(
            ProviderErrorKind::Internal,
            "embedding provider returned an invalid batch shape",
        ));
    }
    if expected == 0 {
        return Ok(());
    }
    // SAFETY: the shape checks above establish a readable provider buffer for
    // the exact callback-reported value count.
    let values = unsafe { slice::from_raw_parts(batch.values, batch.value_count) };
    if values.iter().any(|value| !value.is_finite()) {
        return Err(ProviderError::new(
            ProviderErrorKind::Internal,
            "embedding provider returned a non-finite FLOAT32 coordinate",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    unsafe extern "C" fn valid_validate(
        _context: *mut c_void,
        _config_json: *const u8,
        _config_len: usize,
        _dimensions: usize,
        _coordinate_type: u32,
        _error: *mut EmbeddingErrorV1,
    ) -> c_int {
        EmbeddingStatusV1::Ok.code()
    }

    unsafe extern "C" fn valid_embed_batch(
        _context: *mut c_void,
        _config_json: *const u8,
        _config_len: usize,
        _texts: *const EmbeddingTextV1,
        _text_count: usize,
        _dimensions: usize,
        _coordinate_type: u32,
        _is_cancelled: EmbeddingCancelCallbackV1,
        _cancel_user_data: *mut c_void,
        _result: *mut EmbeddingBatchV1,
        _error: *mut EmbeddingErrorV1,
    ) -> c_int {
        // #lizard forgives(parameter_count)
        EmbeddingStatusV1::Ok.code()
    }

    unsafe extern "C" fn valid_free_batch(_context: *mut c_void, _result: *mut EmbeddingBatchV1) {}

    unsafe extern "C" fn valid_free_error(_context: *mut c_void, _error: *mut EmbeddingErrorV1) {}

    static IDENTITY: &[u8] = b"synthetic/v1";

    fn valid_provider() -> EmbeddingProviderV1 {
        EmbeddingProviderV1 {
            abi_version: EMBEDDING_PROVIDER_ABI_VERSION_V1,
            struct_size: std::mem::size_of::<EmbeddingProviderV1>(),
            context: ptr::null_mut(),
            semantic_identity: IDENTITY.as_ptr(),
            semantic_identity_len: IDENTITY.len(),
            validate: Some(valid_validate),
            embed_batch: Some(valid_embed_batch),
            free_batch: Some(valid_free_batch),
            free_error: Some(valid_free_error),
        }
    }

    #[test]
    fn empty_result_and_error_are_zeroed() {
        let batch = EmbeddingBatchV1::empty();
        assert!(batch.values.is_null());
        assert_eq!(batch.value_count, 0);
        assert_eq!(batch.embedding_count, 0);
        assert_eq!(batch.dimensions, 0);

        let error = EmbeddingErrorV1::empty();
        assert!(error.message.is_null());
        assert_eq!(error.message_len, 0);
        assert_eq!(error.status, EmbeddingStatusV1::Ok.code());
    }

    #[test]
    fn registration_validation_is_fail_closed() {
        let mut provider = valid_provider();
        assert!(validate_registration(&provider, "valid").is_ok());

        provider.struct_size += 64;
        assert!(
            validate_registration(&provider, "larger").is_ok(),
            "forward-compatible larger v1 structs must be accepted"
        );

        provider = valid_provider();
        provider.abi_version += 1;
        assert_eq!(
            validate_registration(&provider, "bad-version")
                .expect_err("bad ABI version")
                .kind,
            ProviderErrorKind::InvalidRegistration
        );

        provider = valid_provider();
        provider.struct_size = std::mem::size_of::<EmbeddingProviderV1>() - 1;
        assert_eq!(
            validate_registration(&provider, "small")
                .expect_err("small ABI struct")
                .kind,
            ProviderErrorKind::InvalidRegistration
        );

        provider = valid_provider();
        provider.semantic_identity = ptr::null();
        assert_eq!(
            validate_registration(&provider, "null-identity")
                .expect_err("null identity")
                .kind,
            ProviderErrorKind::InvalidRegistration
        );

        provider = valid_provider();
        provider.semantic_identity_len = 0;
        assert_eq!(
            validate_registration(&provider, "empty-identity")
                .expect_err("empty identity")
                .kind,
            ProviderErrorKind::InvalidRegistration
        );

        provider = valid_provider();
        provider.semantic_identity_len = MAX_SEMANTIC_IDENTITY_BYTES + 1;
        assert_eq!(
            validate_registration(&provider, "large-identity")
                .expect_err("oversized identity")
                .kind,
            ProviderErrorKind::InvalidRegistration
        );

        provider = valid_provider();
        provider.embed_batch = None;
        assert_eq!(
            validate_registration(&provider, "missing-callback")
                .expect_err("missing callback")
                .kind,
            ProviderErrorKind::InvalidRegistration
        );
    }

    #[test]
    fn undersized_clientdata_is_rejected_before_full_v1_dereference() {
        let mut header = EmbeddingProviderHeaderV1 {
            abi_version: EMBEDDING_PROVIDER_ABI_VERSION_V1,
            struct_size: std::mem::size_of::<EmbeddingProviderHeaderV1>(),
        };
        // SAFETY: this deliberately supplies only the frozen ABI header. The
        // resolver must reject its advertised size before it could form a
        // full EmbeddingProviderV1 reference.
        let result = unsafe {
            provider_from_clientdata(
                ptr::from_mut(&mut header).cast::<c_void>(),
                "undersized-allocation",
            )
        };
        match result {
            Err(error) => assert_eq!(error.kind, ProviderErrorKind::InvalidRegistration),
            Ok(_) => panic!("undersized provider allocation unexpectedly resolved"),
        }
    }

    #[test]
    fn batch_validation_rejects_shape_and_non_finite_values() {
        let empty = EmbeddingBatchV1 {
            values: ptr::null_mut(),
            value_count: 0,
            embedding_count: 0,
            dimensions: 4,
        };
        assert!(validate_batch(&empty, 0, 4).is_ok());

        let mut values = vec![1.0_f32, 2.0, 3.0, 4.0];
        let valid = EmbeddingBatchV1 {
            values: values.as_mut_ptr(),
            value_count: values.len(),
            embedding_count: 2,
            dimensions: 2,
        };
        assert!(validate_batch(&valid, 2, 2).is_ok());

        let null_values = EmbeddingBatchV1 {
            values: ptr::null_mut(),
            value_count: 4,
            embedding_count: 2,
            dimensions: 2,
        };
        assert_eq!(
            validate_batch(&null_values, 2, 2)
                .expect_err("null non-empty output")
                .kind,
            ProviderErrorKind::Internal
        );

        let wrong_shape = EmbeddingBatchV1 {
            values: values.as_mut_ptr(),
            value_count: 3,
            embedding_count: 2,
            dimensions: 2,
        };
        assert_eq!(
            validate_batch(&wrong_shape, 2, 2)
                .expect_err("wrong output shape")
                .kind,
            ProviderErrorKind::Internal
        );

        values[3] = f32::NAN;
        let non_finite = EmbeddingBatchV1 {
            values: values.as_mut_ptr(),
            value_count: values.len(),
            embedding_count: 2,
            dimensions: 2,
        };
        assert_eq!(
            validate_batch(&non_finite, 2, 2)
                .expect_err("non-finite output")
                .kind,
            ProviderErrorKind::Internal
        );
    }

    #[test]
    fn host_checks_cancellation_before_and_after_provider_callback() {
        let provider = valid_provider();
        let registered = RegisteredEmbeddingProvider {
            provider: &provider,
            semantic_identity: "synthetic/v1",
            _connection: PhantomData,
        };
        assert_eq!(
            registered
                .embed_batch(b"{}", &["x"], 1, &|| true)
                .expect_err("pre-call cancellation")
                .kind,
            ProviderErrorKind::Cancelled
        );

        let checks = std::cell::Cell::new(0_u8);
        let late_cancel = || {
            let current = checks.get();
            checks.set(current.saturating_add(1));
            current > 0
        };
        assert_eq!(
            registered
                .embed_batch(b"{}", &["x"], 1, &late_cancel)
                .expect_err("post-call cancellation")
                .kind,
            ProviderErrorKind::Cancelled
        );
        assert_eq!(checks.get(), 2);
    }
}
