//! SQLite loadable-extension boundary for Lithograph.
//!
//! Phase 01 owns the SQLite ABI, initialization metadata, SQL adapter surface,
//! native ABI ownership rules, and the safety boundary. Cypher parsing,
//! versioned graph storage, and query execution are implemented by later
//! phases; this crate must not fake those semantics in order to exercise the
//! adapter.

#![allow(
    unsafe_code,
    reason = "the SQLite loadable-extension and Native ABI boundary necessarily uses raw FFI"
)]
#![cfg_attr(
    not(test),
    deny(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use lithograph_core::{CYPHER_PROFILE, cypher, query, storage};
use rusqlite::OptionalExtension as _;
use rusqlite::vtab::{
    Context as VTabContext, Filters, IndexConstraintOp, IndexInfo, Module, VTab, VTabConfig,
    VTabConnection, VTabCursor,
};
use rusqlite::{Connection, Error as SqliteError, Result as SqliteResult, ffi};
use serde_json::{Value, json};

const ABI_VERSION: u32 = 1;
const STORAGE_FORMAT_MIN: i64 = 1;
const STORAGE_FORMAT_MAX: i64 = 2;
const STORAGE_FORMAT_CURRENT: i64 = 2;
const SQLITE_MIN_VERSION_NUMBER: c_int = 3_045_000;
const META_TABLE: &str = "_lithograph_meta";
const INTERNAL_PREFIX: &str = "_lithograph_";
const MAGIC: &str = "lithograph-format-v1";
const ROWS_MODULE_NAME: &CStr = c"lithograph_rows";

static NEXT_SAVEPOINT: AtomicU64 = AtomicU64::new(1);
static REGISTERED_CONNECTIONS: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();
const EXPLICIT_TRANSACTION_CLIENTDATA_KEY: &CStr = c"lithograph.explicit-transaction.v1";

type SqliteIsInterrupted = unsafe extern "C" fn(*mut ffi::sqlite3) -> c_int;
type SqliteGetClientdata = unsafe extern "C" fn(*mut ffi::sqlite3, *const c_char) -> *mut c_void;
type SqliteSetClientdata = unsafe extern "C" fn(
    *mut ffi::sqlite3,
    *const c_char,
    *mut c_void,
    Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int;

// sqlite3_api_routines is append-only. sqlite3_is_interrupted was appended in
// SQLite 3.41.0 at zero-based table slot 266. Lithograph requires
// SQLite >= 3.45, so every supported loadable-extension host provides it even
// though libsqlite3-sys's conservative default bindings stop at an older slot.
const SQLITE_API_IS_INTERRUPTED_SLOT: usize = 266;
const SQLITE_API_GET_CLIENTDATA_SLOT: usize = 268;
const SQLITE_API_SET_CLIENTDATA_SLOT: usize = 269;
static SQLITE_IS_INTERRUPTED: OnceLock<SqliteIsInterrupted> = OnceLock::new();
static SQLITE_GET_CLIENTDATA: OnceLock<SqliteGetClientdata> = OnceLock::new();
static SQLITE_SET_CLIENTDATA: OnceLock<SqliteSetClientdata> = OnceLock::new();

struct ConnectionRegistration {
    handle: usize,
}

#[derive(Debug, Clone)]
struct ExplicitTransactionState {
    branch: String,
    base_commit: storage::HashId,
    author: Option<String>,
    message: Option<String>,
    started_at_micros: i64,
    mutated: bool,
}

struct ExplicitTransactionSlot {
    state: Option<ExplicitTransactionState>,
}

impl ConnectionRegistration {
    fn new(handle: *mut ffi::sqlite3) -> Self {
        let handle = handle as usize;
        let mut registrations = registered_connections()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        *registrations.entry(handle).or_insert(0) += 1;
        Self { handle }
    }
}

impl Drop for ConnectionRegistration {
    fn drop(&mut self) {
        let mut registrations = registered_connections()
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        if let Some(count) = registrations.get_mut(&self.handle) {
            if *count <= 1 {
                registrations.remove(&self.handle);
            } else {
                *count -= 1;
            }
        }
    }
}

fn registered_connections() -> &'static Mutex<HashMap<usize, usize>> {
    REGISTERED_CONNECTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn explicit_transaction_state(handle: *mut ffi::sqlite3) -> Option<ExplicitTransactionState> {
    if handle.is_null() {
        return None;
    }
    let slot = host_get_clientdata(handle, EXPLICIT_TRANSACTION_CLIENTDATA_KEY)
        .cast::<ExplicitTransactionSlot>();
    if slot.is_null() {
        return None;
    }
    // SAFETY: this key is only populated with `ExplicitTransactionSlot` below.
    unsafe { (*slot).state.clone() }
}

unsafe extern "C" fn destroy_explicit_transaction_slot(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    // SAFETY: SQLite invokes this destructor exactly once for the Box pointer
    // registered by `store_explicit_transaction_state`.
    drop(unsafe { Box::from_raw(pointer.cast::<ExplicitTransactionSlot>()) });
}

fn store_explicit_transaction_state(
    handle: *mut ffi::sqlite3,
    state: ExplicitTransactionState,
) -> LithographResult<()> {
    let slot = host_get_clientdata(handle, EXPLICIT_TRANSACTION_CLIENTDATA_KEY)
        .cast::<ExplicitTransactionSlot>();
    if !slot.is_null() {
        // SAFETY: this key is only populated with `ExplicitTransactionSlot`.
        unsafe { (*slot).state = Some(state) };
        return Ok(());
    }
    let slot = Box::into_raw(Box::new(ExplicitTransactionSlot { state: Some(state) }));
    let code = host_set_clientdata(
        handle,
        EXPLICIT_TRANSACTION_CLIENTDATA_KEY,
        slot.cast::<c_void>(),
        Some(destroy_explicit_transaction_slot),
    );
    if code == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(map_sqlite_error(
            SqliteError::SqliteFailure(ffi::Error::new(code), None),
            "failed to attach explicit transaction state to SQLite connection",
        ))
    }
}

fn update_explicit_transaction_state(
    handle: *mut ffi::sqlite3,
    update: impl FnOnce(&mut ExplicitTransactionState) -> LithographResult<()>,
) -> LithographResult<()> {
    let slot = host_get_clientdata(handle, EXPLICIT_TRANSACTION_CLIENTDATA_KEY)
        .cast::<ExplicitTransactionSlot>();
    if slot.is_null() {
        return Err(LithographError::internal(
            "explicit transaction state disappeared during execution",
        ));
    }
    // SAFETY: this key is only populated with `ExplicitTransactionSlot`.
    let state = unsafe { (*slot).state.as_mut() }.ok_or_else(|| {
        LithographError::internal("explicit transaction state disappeared during execution")
    })?;
    update(state)
}

fn clear_explicit_transaction_state(handle: *mut ffi::sqlite3) {
    if handle.is_null() {
        return;
    }
    let slot = host_get_clientdata(handle, EXPLICIT_TRANSACTION_CLIENTDATA_KEY)
        .cast::<ExplicitTransactionSlot>();
    if !slot.is_null() {
        // SAFETY: this key is only populated with `ExplicitTransactionSlot`.
        unsafe { (*slot).state = None };
    }
}

fn require_no_explicit_transaction(connection: &Connection) -> LithographResult<()> {
    // SAFETY: `connection` is live for this call.
    let handle = unsafe { connection.handle() };
    if explicit_transaction_state(handle).is_some() {
        return Err(LithographError::new(
            ErrorCategory::TransactionBoundaryRequired,
            "an explicit Lithograph transaction is active on this connection",
            ffi::SQLITE_ERROR,
        ));
    }
    Ok(())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorCategory {
    Parse,
    Semantic,
    Type,
    Schema,
    Constraint,
    NotInitialized,
    InvalidArgument,
    VersionNotFound,
    BranchNotFound,
    TagNotFound,
    GraphViewViolation,
    BranchHeadMoved,
    MergeSessionNotFound,
    MergeSessionChanged,
    MergeConflict,
    ReadOnlyAdapter,
    ReadOnlySnapshot,
    TransactionBoundaryRequired,
    FormatTooNew,
    Busy,
    Resource,
    Storage,
    Io,
    Internal,
}

impl ErrorCategory {
    fn as_str(self) -> &'static str {
        match self {
            Self::Parse => "PARSE_ERROR",
            Self::Semantic => "SEMANTIC_ERROR",
            Self::Type => "TYPE_ERROR",
            Self::Schema => "SCHEMA_ERROR",
            Self::Constraint => "CONSTRAINT_ERROR",
            Self::NotInitialized => "NOT_INITIALIZED",
            Self::InvalidArgument => "INVALID_ARGUMENT",
            Self::VersionNotFound => "VERSION_NOT_FOUND",
            Self::BranchNotFound => "BRANCH_NOT_FOUND",
            Self::TagNotFound => "TAG_NOT_FOUND",
            Self::GraphViewViolation => "GRAPH_VIEW_VIOLATION",
            Self::BranchHeadMoved => "BRANCH_HEAD_MOVED",
            Self::MergeSessionNotFound => "MERGE_SESSION_NOT_FOUND",
            Self::MergeSessionChanged => "MERGE_SESSION_CHANGED",
            Self::MergeConflict => "MERGE_CONFLICT",
            Self::ReadOnlyAdapter => "READ_ONLY_ADAPTER",
            Self::ReadOnlySnapshot => "READ_ONLY_SNAPSHOT",
            Self::TransactionBoundaryRequired => "TRANSACTION_BOUNDARY_REQUIRED",
            Self::FormatTooNew => "FORMAT_TOO_NEW",
            Self::Busy => "BUSY",
            Self::Resource => "RESOURCE_ERROR",
            Self::Storage => "STORAGE_ERROR",
            Self::Io => "IO_ERROR",
            Self::Internal => "INTERNAL_ERROR",
        }
    }
}

#[derive(Debug, Clone)]
struct LithographError {
    category: ErrorCategory,
    message: String,
    sqlite_code: c_int,
    line: Option<u64>,
    column: Option<u64>,
}

type LithographResult<T> = std::result::Result<T, LithographError>;

impl LithographError {
    fn new(category: ErrorCategory, message: impl Into<String>, sqlite_code: c_int) -> Self {
        Self {
            category,
            message: message.into(),
            sqlite_code,
            line: None,
            column: None,
        }
    }

    fn invalid_argument(message: impl Into<String>) -> Self {
        Self::new(ErrorCategory::InvalidArgument, message, ffi::SQLITE_ERROR)
    }

    fn not_initialized() -> Self {
        Self::new(
            ErrorCategory::NotInitialized,
            "database has not been initialized with lithograph_init()",
            ffi::SQLITE_ERROR,
        )
    }

    fn storage(message: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Storage, message, ffi::SQLITE_ERROR)
    }

    fn internal(message: impl Into<String>) -> Self {
        Self::new(ErrorCategory::Internal, message, ffi::SQLITE_ERROR)
    }

    fn to_sqlite_error(&self) -> SqliteError {
        SqliteError::SqliteFailure(
            ffi::Error::new(self.sqlite_code),
            Some(format!(
                "LITHOGRAPH_{}: {}",
                self.category.as_str(),
                self.message
            )),
        )
    }

    fn to_json_value(&self) -> Value {
        json!({
            "category": self.category.as_str(),
            "message": self.message,
            "sqliteCode": self.sqlite_code,
            "line": self.line,
            "column": self.column,
        })
    }

    fn to_json(&self) -> String {
        self.to_json_value().to_string()
    }
}

#[derive(Debug, Clone)]
struct Metadata {
    database_id: String,
    storage_format: i64,
}

/// Entry point used by stock SQLite when loading the Lithograph shared library.
///
/// # Safety
///
/// SQLite calls this function with pointers governed by the loadable-extension
/// ABI. The pointers must originate from the SQLite host loading this library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_lithograph_init(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    // SAFETY: SQLite is the only caller of this entry point and supplies all
    // pointers according to the loadable-extension ABI documented above.
    match catch_unwind(AssertUnwindSafe(|| {
        if !host_sqlite_version_supported(p_api) {
            return ffi::SQLITE_ERROR;
        }
        // SAFETY: SQLite owns `db`, `pz_err_msg`, and `p_api` for this init
        // call and requires extensions to initialize their API thunk table
        // before invoking SQLite services.
        let code = unsafe { Connection::extension_init2(db, pz_err_msg, p_api, extension_init) };
        if code == ffi::SQLITE_OK && !capture_required_host_apis(p_api) {
            return ffi::SQLITE_ERROR;
        }
        code
    })) {
        Ok(code) => code,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

fn host_sqlite_version_supported(p_api: *mut ffi::sqlite3_api_routines) -> bool {
    host_sqlite_version_number(p_api).is_some_and(sqlite_version_number_supported)
}

fn sqlite_version_number_supported(version: c_int) -> bool {
    version >= SQLITE_MIN_VERSION_NUMBER
}

fn host_sqlite_version_number(p_api: *mut ffi::sqlite3_api_routines) -> Option<c_int> {
    if p_api.is_null() {
        return None;
    }
    // SAFETY: `libversion_number` is part of the original extension API table
    // prefix and is therefore readable before touching any newer append-only
    // slots. This guard prevents out-of-bounds access on unsupported hosts.
    let function = unsafe { (*p_api).libversion_number }?;
    // SAFETY: the function pointer comes from the live host API table.
    Some(unsafe { function() })
}

fn capture_required_host_apis(p_api: *mut ffi::sqlite3_api_routines) -> bool {
    if SQLITE_IS_INTERRUPTED.get().is_some()
        && SQLITE_GET_CLIENTDATA.get().is_some()
        && SQLITE_SET_CLIENTDATA.get().is_some()
    {
        return true;
    }
    if p_api.is_null() {
        return false;
    }

    // SQLite's loadable-extension ABI represents sqlite3_api_routines as an
    // append-only array of function pointers. The host allocation is larger
    // than the conservative Rust binding type when loading on SQLite >= 3.41.
    let slots = p_api.cast::<*const c_void>();
    // SAFETY: supported hosts are SQLite >= 3.45, whose append-only API table
    // contains slots 266 (`is_interrupted`) and 268/269 (client data).
    let interrupted_slot = unsafe { slots.add(SQLITE_API_IS_INTERRUPTED_SLOT) };
    // SAFETY: the slot points inside the live host-owned API table.
    let interrupted = unsafe { interrupted_slot.read() };
    // SAFETY: same host-owned API table and minimum SQLite version as above.
    let get_clientdata_slot = unsafe { slots.add(SQLITE_API_GET_CLIENTDATA_SLOT) };
    // SAFETY: the slot points inside the live host-owned API table.
    let get_clientdata = unsafe { get_clientdata_slot.read() };
    // SAFETY: same host-owned API table and minimum SQLite version as above.
    let set_clientdata_slot = unsafe { slots.add(SQLITE_API_SET_CLIENTDATA_SLOT) };
    // SAFETY: the slot points inside the live host-owned API table.
    let set_clientdata = unsafe { set_clientdata_slot.read() };
    if interrupted.is_null() || get_clientdata.is_null() || set_clientdata.is_null() {
        return false;
    }
    // SAFETY: SQLite documents these append-only slots with the signatures
    // represented by the aliases above.
    let interrupted =
        unsafe { std::mem::transmute::<*const c_void, SqliteIsInterrupted>(interrupted) };
    // SAFETY: see the API-slot contract above.
    let get_clientdata =
        unsafe { std::mem::transmute::<*const c_void, SqliteGetClientdata>(get_clientdata) };
    // SAFETY: see the API-slot contract above.
    let set_clientdata =
        unsafe { std::mem::transmute::<*const c_void, SqliteSetClientdata>(set_clientdata) };
    let _ = SQLITE_IS_INTERRUPTED.set(interrupted);
    let _ = SQLITE_GET_CLIENTDATA.set(get_clientdata);
    let _ = SQLITE_SET_CLIENTDATA.set(set_clientdata);
    true
}

fn host_is_interrupted(db: *mut ffi::sqlite3) -> bool {
    let Some(function) = SQLITE_IS_INTERRUPTED.get() else {
        return false;
    };
    // SAFETY: the function pointer was captured from SQLite's process-lifetime
    // extension API table, and callers supply a live SQLite connection handle.
    unsafe { function(db) != 0 }
}

fn host_get_clientdata(db: *mut ffi::sqlite3, key: &CStr) -> *mut c_void {
    let Some(function) = SQLITE_GET_CLIENTDATA.get() else {
        return ptr::null_mut();
    };
    // SAFETY: the function pointer comes from the host API table, `db` is a
    // live SQLite connection, and `key` is NUL-terminated for the call.
    unsafe { function(db, key.as_ptr()) }
}

fn host_set_clientdata(
    db: *mut ffi::sqlite3,
    key: &CStr,
    data: *mut c_void,
    destructor: Option<unsafe extern "C" fn(*mut c_void)>,
) -> c_int {
    let Some(function) = SQLITE_SET_CLIENTDATA.get() else {
        return ffi::SQLITE_MISUSE;
    };
    // SAFETY: the function pointer comes from the host API table; SQLite owns
    // the client-data lifecycle after a successful registration.
    unsafe { function(db, key.as_ptr(), data, destructor) }
}

fn extension_init(db: Connection) -> SqliteResult<bool> {
    storage::initialize_connection_state(&db)
        .map_err(|error| rusqlite::Error::ModuleError(error.to_string()))?;
    register_scalar_functions(&db)?;

    const ROWS_MODULE: Module<'static, RowsTab> = Module::eponymous_only_module();
    // SAFETY: `db` is a live rusqlite connection for the duration of module
    // registration; the raw handle is stored only to identify this connection.
    let handle = unsafe { db.handle() };
    let registration = ConnectionRegistration::new(handle);
    db.create_module(ROWS_MODULE_NAME, &ROWS_MODULE, Some(registration))?;

    Ok(false)
}

fn initialize(connection: &Connection) -> LithographResult<Value> {
    match read_metadata(connection) {
        Ok(Some(metadata)) => initialize_existing(connection, metadata),
        Ok(None) => initialize_fresh(connection),
        Err(error) => Err(error),
    }
}

fn initialize_existing(connection: &Connection, mut metadata: Metadata) -> LithographResult<Value> {
    ensure_supported_format(&metadata)?;
    let bootstrapped = migrate_phase01_bootstrap(connection)?;
    if metadata.storage_format == 1 {
        migrate_storage_format_1_to_2(connection, bootstrapped)?;
        metadata.storage_format = 2;
    }
    ensure_current_metadata_integrity(connection)?;
    let root = storage::root_commit(connection)
        .map_err(|error| map_storage_error(error, "failed to resolve Root Commit"))?;
    storage::branch_head(connection, "main")
        .map_err(|error| map_storage_error(error, "failed to resolve main branch"))?;
    Ok(init_json(&metadata, root))
}

fn migrate_phase01_bootstrap(connection: &Connection) -> LithographResult<bool> {
    if !is_phase01_metadata_bootstrap(connection)? {
        return Ok(false);
    }
    storage::create_storage_schema(connection).map_err(|error| {
        map_storage_error(error, "failed to migrate Phase 01 storage bootstrap")
    })?;
    storage::initialize_root(connection).map_err(|error| {
        map_storage_error(error, "failed to initialize Root Commit during migration")
    })?;
    Ok(true)
}

fn migrate_storage_format_1_to_2(
    connection: &Connection,
    sidecars_already_created: bool,
) -> LithographResult<()> {
    with_savepoint(connection, |connection| {
        if !sidecars_already_created {
            storage::create_format2_schema(connection).map_err(|error| {
                map_storage_error(error, "failed to create storage-format-2 schema")
            })?;
        }
        let changed = connection
            .execute(
                "UPDATE main._lithograph_meta SET storage_format = 2 WHERE id = 1 AND storage_format = 1",
                [],
            )
            .map_err(|error| {
                map_sqlite_error(error, "failed to advance Lithograph storage format")
            })?;
        if changed != 1 {
            return Err(LithographError::storage(
                "storage-format-1 migration lost the metadata compare-and-swap",
            ));
        }
        Ok(())
    })
}

fn initialize_fresh(connection: &Connection) -> LithographResult<Value> {
    if has_any_internal_object(connection)? {
        return Err(LithographError::storage(
            "reserved _lithograph_* object exists without a valid Lithograph marker",
        ));
    }
    create_metadata_table(connection)?;
    let metadata = Metadata {
        database_id: generate_database_id(connection)?,
        storage_format: STORAGE_FORMAT_CURRENT,
    };
    connection
        .execute(
            "INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format) VALUES(1, ?1, ?2, ?3)",
            rusqlite::params![MAGIC, metadata.database_id, metadata.storage_format],
        )
        .map_err(|error| map_sqlite_error(error, "failed to persist Lithograph metadata"))?;
    storage::create_storage_schema(connection)
        .map_err(|error| map_storage_error(error, "failed to create Lithograph storage schema"))?;
    let root = storage::initialize_root(connection).map_err(|error| {
        map_storage_error(error, "failed to initialize Root Commit and main branch")
    })?;
    ensure_current_metadata_integrity(connection)?;
    Ok(init_json(&metadata, root.root))
}

fn init_json(metadata: &Metadata, root: storage::HashId) -> Value {
    json!({
        "databaseId": metadata.database_id,
        "storageFormat": metadata.storage_format,
        "root": root.to_hex(),
        "branch": "main",
    })
}

fn version_json(connection: &Connection) -> LithographResult<Value> {
    let metadata = read_metadata(connection)?;
    let Some(metadata) = metadata else {
        if has_any_internal_object(connection)? {
            return Err(LithographError::storage(
                "reserved _lithograph_ schema evidence exists without valid metadata",
            ));
        }
        return Ok(json!({
            "extension": env!("CARGO_PKG_VERSION"),
            "abi": ABI_VERSION,
            "cypherProfile": CYPHER_PROFILE,
            "storageFormat": {
                "min": STORAGE_FORMAT_MIN,
                "max": STORAGE_FORMAT_MAX,
                "current": Value::Null,
            },
            "databaseId": Value::Null,
        }));
    };
    if metadata.storage_format < STORAGE_FORMAT_MIN {
        return Err(LithographError::storage(format!(
            "database storage format {} is below supported minimum {} and has no migration path",
            metadata.storage_format, STORAGE_FORMAT_MIN
        )));
    }
    if metadata.storage_format <= STORAGE_FORMAT_MAX {
        ensure_current_metadata_integrity(connection)?;
    }

    Ok(json!({
        "extension": env!("CARGO_PKG_VERSION"),
        "abi": ABI_VERSION,
        "cypherProfile": CYPHER_PROFILE,
        "storageFormat": {
            "min": STORAGE_FORMAT_MIN,
            "max": STORAGE_FORMAT_MAX,
            "current": metadata.storage_format,
        },
        "databaseId": metadata.database_id,
    }))
}

fn create_metadata_table(connection: &Connection) -> LithographResult<()> {
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                id INTEGER PRIMARY KEY CHECK(id = 1),\
                magic TEXT NOT NULL,\
                database_id TEXT NOT NULL,\
                storage_format INTEGER NOT NULL\
            );",
        )
        .map_err(|error| map_sqlite_error(error, "failed to create Lithograph metadata"))
}

fn read_metadata(connection: &Connection) -> LithographResult<Option<Metadata>> {
    let object_type = metadata_object_type(connection)?;

    let Some(object_type) = object_type else {
        return Ok(None);
    };
    if object_type != "table" {
        return Err(LithographError::storage(
            "Lithograph metadata has an invalid storage object type",
        ));
    }

    let row = query_metadata_marker(connection)
        .map_err(|error| map_sqlite_error(error, "failed to read Lithograph metadata"))?;

    let Some((magic, database_id, storage_format)) = row else {
        return Err(LithographError::storage(
            "Lithograph metadata is missing its marker row",
        ));
    };
    if magic != MAGIC || !is_canonical_uuid(&database_id) {
        return Err(LithographError::storage(
            "Lithograph metadata does not contain a valid marker",
        ));
    }

    Ok(Some(Metadata {
        database_id,
        storage_format,
    }))
}

fn require_initialized(connection: &Connection) -> LithographResult<Metadata> {
    let metadata = match read_metadata(connection) {
        Ok(Some(metadata)) => metadata,
        Ok(None) => return Err(LithographError::not_initialized()),
        Err(error) => return Err(error),
    };
    ensure_supported_format(&metadata)?;
    ensure_current_metadata_integrity(connection)?;
    Ok(metadata)
}

fn ensure_supported_format(metadata: &Metadata) -> LithographResult<()> {
    if metadata.storage_format > STORAGE_FORMAT_MAX {
        return Err(format_too_new_error(metadata.storage_format));
    }
    if metadata.storage_format < STORAGE_FORMAT_MIN {
        return Err(LithographError::storage(format!(
            "database storage format {} is below supported minimum {}",
            metadata.storage_format, STORAGE_FORMAT_MIN
        )));
    }
    Ok(())
}

fn format_too_new_error(storage_format: i64) -> LithographError {
    LithographError::new(
        ErrorCategory::FormatTooNew,
        format!(
            "database storage format {storage_format} is newer than supported maximum {STORAGE_FORMAT_MAX}"
        ),
        ffi::SQLITE_ERROR,
    )
}

fn generate_database_id(connection: &Connection) -> LithographResult<String> {
    let mut bytes = connection
        .query_row("SELECT randomblob(16)", [], |row| row.get::<_, Vec<u8>>(0))
        .map_err(|error| map_sqlite_error(error, "failed to generate databaseId"))?;
    if bytes.len() != 16 {
        return Err(LithographError::internal(
            "SQLite random generator returned an invalid UUID payload",
        ));
    }

    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let mut output = String::with_capacity(36);
    for (index, byte) in bytes.iter().enumerate() {
        if matches!(index, 4 | 6 | 8 | 10) {
            output.push('-');
        }
        write!(&mut output, "{byte:02x}")
            .map_err(|_| LithographError::internal("failed to format generated databaseId"))?;
    }
    Ok(output)
}

fn is_canonical_uuid(value: &str) -> bool {
    let bytes = value.as_bytes();
    if bytes.len() != 36 {
        return false;
    }
    for (index, byte) in bytes.iter().copied().enumerate() {
        if matches!(index, 8 | 13 | 18 | 23) {
            if byte != b'-' {
                return false;
            }
        } else if !byte.is_ascii_digit() && !(b'a'..=b'f').contains(&byte) {
            return false;
        }
    }
    bytes[14] == b'4' && matches!(bytes[19], b'8' | b'9' | b'a' | b'b')
}

#[cfg(test)]
fn validate_json_object(input: &str, name: &str) -> LithographResult<Value> {
    let value: Value = serde_json::from_str(input).map_err(|_| {
        LithographError::invalid_argument(format!("{name} must contain valid JSON"))
    })?;
    if !value.is_object() {
        return Err(LithographError::invalid_argument(format!(
            "{name} must be a JSON object"
        )));
    }
    Ok(value)
}

fn with_savepoint<T>(
    connection: &Connection,
    operation: impl FnOnce(&Connection) -> LithographResult<T>,
) -> LithographResult<T> {
    let ordinal = NEXT_SAVEPOINT.fetch_add(1, Ordering::Relaxed);
    let savepoint = format!("lithograph_invocation_{ordinal}");
    connection
        .execute_batch(&format!("SAVEPOINT {savepoint}"))
        .map_err(|error| map_sqlite_error(error, "failed to start internal savepoint"))?;

    let outcome = catch_unwind(AssertUnwindSafe(|| operation(connection)));
    match outcome {
        Ok(Ok(value)) => match connection.execute_batch(&format!("RELEASE {savepoint}")) {
            Ok(()) => Ok(value),
            Err(error) => {
                let release_error = map_sqlite_error(error, "failed to release internal savepoint");
                rollback_savepoint(connection, &savepoint)?;
                Err(release_error)
            }
        },
        Ok(Err(error)) => {
            rollback_savepoint(connection, &savepoint)?;
            Err(error)
        }
        Err(_) => {
            rollback_savepoint(connection, &savepoint)?;
            Err(LithographError::internal(
                "panic while executing a Lithograph invocation",
            ))
        }
    }
}

fn rollback_savepoint(connection: &Connection, savepoint: &str) -> LithographResult<()> {
    let rollback_error = connection
        .execute_batch(&format!("ROLLBACK TO {savepoint}"))
        .err();

    let release_error = if rollback_error.is_none() {
        match connection.execute_batch(&format!("RELEASE {savepoint}")) {
            Ok(()) => return Ok(()),
            Err(error) => Some(error),
        }
    } else {
        None
    };

    let detail = match (rollback_error, release_error) {
        (Some(rollback), _) => format!("rollback-to-savepoint failed ({rollback})"),
        (None, Some(release)) => format!("release-savepoint failed ({release})"),
        (None, None) => "unknown cleanup failure".to_owned(),
    };

    // Never RELEASE after a failed ROLLBACK TO: for an outermost savepoint,
    // RELEASE could commit the very changes this cleanup path is trying to
    // discard. A full rollback is the only fail-closed recovery available
    // when SQLite refuses normal savepoint cleanup. Even if it succeeds, the
    // caller must be told that invocation-local transaction semantics could
    // not be preserved (and an outer transaction may have been aborted).
    if connection.execute_batch("ROLLBACK").is_ok() {
        return Err(LithographError::internal(format!(
            "internal savepoint cleanup failed ({detail}); full SQLite rollback executed"
        )));
    }

    Err(LithographError::internal(format!(
        "internal savepoint cleanup failed ({detail}); full SQLite rollback also failed"
    )))
}

fn map_frontend_error(error: cypher::FrontendError) -> LithographError {
    let category = match error.kind {
        cypher::FrontendErrorKind::Parse => ErrorCategory::Parse,
        cypher::FrontendErrorKind::Semantic => ErrorCategory::Semantic,
        cypher::FrontendErrorKind::Type => ErrorCategory::Type,
        cypher::FrontendErrorKind::Schema => ErrorCategory::Schema,
        cypher::FrontendErrorKind::InvalidArgument => ErrorCategory::InvalidArgument,
    };
    let mut mapped = LithographError::new(category, error.message, ffi::SQLITE_ERROR);
    mapped.line = Some(u64::from(error.line));
    mapped.column = Some(u64::from(error.column));
    mapped
}

fn validate_cypher(query: &str) -> LithographResult<Value> {
    cypher::validate(query).map_err(map_frontend_error)?;
    Ok(json!({
        "valid": true,
        "cypherProfile": CYPHER_PROFILE,
    }))
}

fn map_storage_error(error: storage::StorageError, message: &str) -> LithographError {
    match error {
        storage::StorageError::Sqlite(error) => map_sqlite_error(error, message),
        other => LithographError::storage(format!("{message}: {other}")),
    }
}

fn map_sqlite_error(error: SqliteError, message: &str) -> LithographError {
    let primary = match &error {
        SqliteError::SqliteFailure(error, _) => error.extended_code & 0xff,
        _ => ffi::SQLITE_ERROR,
    };
    let category = match primary {
        ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED => ErrorCategory::Busy,
        ffi::SQLITE_NOMEM | ffi::SQLITE_TOOBIG | ffi::SQLITE_FULL => ErrorCategory::Resource,
        ffi::SQLITE_IOERR | ffi::SQLITE_CANTOPEN | ffi::SQLITE_READONLY => ErrorCategory::Io,
        _ => ErrorCategory::Storage,
    };
    LithographError::new(category, message, primary)
}

fn catch_sqlite_boundary<T>(operation: impl FnOnce() -> SqliteResult<T>) -> SqliteResult<T> {
    match catch_unwind(AssertUnwindSafe(operation)) {
        Ok(result) => result,
        Err(_) => Err(LithographError::internal(
            "panic while executing a SQLite virtual-table callback",
        )
        .to_sqlite_error()),
    }
}

mod execution;
mod metadata_integrity;
mod native;
mod rows;
mod scalar;

use metadata_integrity::{
    ensure_current_metadata_integrity, has_any_internal_object, is_phase01_metadata_bootstrap,
    metadata_integrity_json, metadata_object_type, query_metadata_marker,
};

#[cfg(test)]
use native::input_utf8;
pub use native::{
    LithographEventCallbackV1, LithographEventKindV1, lithograph_v1_execute, lithograph_v1_free,
    lithograph_v1_tx_abort, lithograph_v1_tx_begin, lithograph_v1_tx_commit,
    lithograph_v1_tx_execute, lithograph_v1_validate,
};
use rows::RowsTab;
use scalar::register_scalar_functions;

#[cfg(test)]
mod tests;
