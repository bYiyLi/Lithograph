//! SQLite loadable-extension boundary for Lithograph.
//!
//! Phase 01 owns the SQLite ABI, initialization metadata, SQL adapter surface,
//! native ABI ownership rules, and the safety boundary. Cypher parsing,
//! versioned graph storage, and query execution are implemented by later
//! phases; this crate must not fake those semantics in order to exercise the
//! adapter.

use std::borrow::Cow;
use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt::Write as _;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Mutex, OnceLock};

use lithograph_core::CYPHER_PROFILE;
use rusqlite::OptionalExtension as _;
use rusqlite::functions::{Context as FunctionContext, FunctionFlags};
use rusqlite::vtab::{
    Context as VTabContext, Filters, IndexConstraintOp, IndexInfo, Module, VTab, VTabConfig,
    VTabConnection, VTabCursor,
};
use rusqlite::{Connection, Error as SqliteError, Result as SqliteResult, ffi};
use serde_json::{Value, json};

const ABI_VERSION: u32 = 1;
const STORAGE_FORMAT_MIN: i64 = 1;
const STORAGE_FORMAT_MAX: i64 = 1;
const STORAGE_FORMAT_CURRENT: i64 = 1;
const META_TABLE: &str = "_lithograph_meta";
const MAGIC: &str = "lithograph-format-v1";
const ROWS_MODULE_NAME: &CStr = c"lithograph_rows";

static NEXT_SAVEPOINT: AtomicU64 = AtomicU64::new(1);
static REGISTERED_CONNECTIONS: OnceLock<Mutex<HashMap<usize, usize>>> = OnceLock::new();

struct ConnectionRegistration {
    handle: usize,
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ErrorCategory {
    Semantic,
    NotInitialized,
    InvalidArgument,
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
            Self::Semantic => "SEMANTIC_ERROR",
            Self::NotInitialized => "NOT_INITIALIZED",
            Self::InvalidArgument => "INVALID_ARGUMENT",
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

    fn semantic_unavailable() -> Self {
        Self::new(
            ErrorCategory::Semantic,
            "Cypher frontend and execution are not available in this development phase",
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
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        Connection::extension_init2(db, pz_err_msg, p_api, extension_init)
    })) {
        Ok(code) => code,
        Err(_) => ffi::SQLITE_ERROR,
    }
}

fn extension_init(db: Connection) -> SqliteResult<bool> {
    let direct = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_DIRECTONLY;
    let innocuous = FunctionFlags::SQLITE_UTF8 | FunctionFlags::SQLITE_INNOCUOUS;

    db.create_scalar_function("lithograph_init", 0, direct, sql_init)?;
    db.create_scalar_function("lithograph", -1, direct, sql_execute)?;
    db.create_scalar_function("lithograph_validate", 1, innocuous, sql_validate)?;
    db.create_scalar_function("lithograph_version", 0, innocuous, sql_version)?;
    db.create_scalar_function(
        "lithograph_integrity_check",
        0,
        innocuous,
        sql_integrity_check,
    )?;

    const ROWS_MODULE: Module<'static, RowsTab> = Module::eponymous_only_module();
    let handle = unsafe { db.handle() };
    let registration = ConnectionRegistration::new(handle);
    db.create_module(ROWS_MODULE_NAME, &ROWS_MODULE, Some(registration))?;

    Ok(false)
}

fn sql_init(ctx: &FunctionContext<'_>) -> SqliteResult<String> {
    let connection = unsafe { ctx.get_connection() }.map_err(|error| {
        map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
    })?;
    with_savepoint(&connection, initialize)
        .map(|value| value.to_string())
        .map_err(|error| error.to_sqlite_error())
}

fn sql_execute(ctx: &FunctionContext<'_>) -> SqliteResult<String> {
    let (query, params, options) = scalar_execution_args(ctx).map_err(|e| e.to_sqlite_error())?;
    validate_json_object(&params, "params").map_err(|e| e.to_sqlite_error())?;
    validate_json_object(&options, "options").map_err(|e| e.to_sqlite_error())?;
    if query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty").to_sqlite_error());
    }
    let connection = unsafe { ctx.get_connection() }.map_err(|error| {
        map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
    })?;
    require_initialized(&connection).map_err(|e| e.to_sqlite_error())?;

    Err(LithographError::semantic_unavailable().to_sqlite_error())
}

fn sql_validate(ctx: &FunctionContext<'_>) -> SqliteResult<String> {
    let query = ctx
        .get::<String>(0)
        .map_err(|_| LithographError::invalid_argument("query must be TEXT").to_sqlite_error())?;
    if query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty").to_sqlite_error());
    }
    let connection = unsafe { ctx.get_connection() }.map_err(|error| {
        map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
    })?;
    require_initialized(&connection).map_err(|e| e.to_sqlite_error())?;

    Err(LithographError::semantic_unavailable().to_sqlite_error())
}

fn sql_version(ctx: &FunctionContext<'_>) -> SqliteResult<String> {
    let connection = unsafe { ctx.get_connection() }.map_err(|error| {
        map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
    })?;
    version_json(&connection)
        .map(|value| value.to_string())
        .map_err(|error| error.to_sqlite_error())
}

fn sql_integrity_check(ctx: &FunctionContext<'_>) -> SqliteResult<String> {
    let connection = unsafe { ctx.get_connection() }.map_err(|error| {
        map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
    })?;
    metadata_integrity_json(&connection)
        .map(|value| value.to_string())
        .map_err(|error| error.to_sqlite_error())
}

fn scalar_execution_args(ctx: &FunctionContext<'_>) -> LithographResult<(String, String, String)> {
    match ctx.len() {
        1..=3 => {}
        _ => {
            return Err(LithographError::invalid_argument(
                "lithograph() expects query [, params [, options]]",
            ));
        }
    }

    let query = ctx
        .get::<String>(0)
        .map_err(|_| LithographError::invalid_argument("query must be TEXT"))?;
    let params = if ctx.len() >= 2 {
        ctx.get::<String>(1)
            .map_err(|_| LithographError::invalid_argument("params must be JSON TEXT"))?
    } else {
        "{}".to_owned()
    };
    let options = if ctx.len() >= 3 {
        ctx.get::<String>(2)
            .map_err(|_| LithographError::invalid_argument("options must be JSON TEXT"))?
    } else {
        "{}".to_owned()
    };

    Ok((query, params, options))
}

fn initialize(connection: &Connection) -> LithographResult<Value> {
    match read_metadata(connection) {
        Ok(Some(metadata)) => {
            ensure_supported_format(&metadata)?;
            ensure_current_metadata_integrity(connection)?;
            Ok(init_json(&metadata))
        }
        Ok(None) => {
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
            Ok(init_json(&metadata))
        }
        Err(error) => Err(error),
    }
}

fn init_json(metadata: &Metadata) -> Value {
    json!({
        "databaseId": metadata.database_id,
        "storageFormat": metadata.storage_format,
        "root": Value::Null,
        "branch": Value::Null,
    })
}

fn version_json(connection: &Connection) -> LithographResult<Value> {
    let metadata = read_metadata(connection)?;
    if let Some(metadata) = metadata.as_ref()
        && (STORAGE_FORMAT_MIN..=STORAGE_FORMAT_MAX).contains(&metadata.storage_format)
    {
        ensure_current_metadata_integrity(connection)?;
    }
    let current = metadata.as_ref().map(|metadata| metadata.storage_format);
    let database_id = metadata.map(|metadata| metadata.database_id);

    Ok(json!({
        "extension": env!("CARGO_PKG_VERSION"),
        "abi": ABI_VERSION,
        "cypherProfile": CYPHER_PROFILE,
        "storageFormat": {
            "min": STORAGE_FORMAT_MIN,
            "max": STORAGE_FORMAT_MAX,
            "current": current,
        },
        "databaseId": database_id,
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

fn has_any_internal_object(connection: &Connection) -> LithographResult<bool> {
    connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE name GLOB '_lithograph_*')",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(|error| map_sqlite_error(error, "failed to inspect database schema"))
}

fn read_metadata(connection: &Connection) -> LithographResult<Option<Metadata>> {
    let object_type = connection
        .query_row(
            "SELECT type FROM main.sqlite_schema WHERE name = ?1 LIMIT 1",
            [META_TABLE],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata"))?;

    let Some(object_type) = object_type else {
        return Ok(None);
    };
    if object_type != "table" {
        return Err(LithographError::storage(
            "Lithograph metadata has an invalid storage object type",
        ));
    }

    let row = connection
        .query_row(
            "SELECT magic, database_id, storage_format FROM main._lithograph_meta WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional()
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

#[derive(Debug, PartialEq, Eq)]
struct MetadataColumn {
    name: String,
    declared_type: String,
    not_null: i64,
    default_value: Option<String>,
    primary_key: i64,
    hidden: i64,
}

fn metadata_integrity_json(connection: &Connection) -> LithographResult<Value> {
    let object_type = connection
        .query_row(
            "SELECT type FROM main.sqlite_schema WHERE name = ?1 LIMIT 1",
            [META_TABLE],
            |row| row.get::<_, String>(0),
        )
        .optional()
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata"))?;

    let Some(object_type) = object_type else {
        return Err(LithographError::not_initialized());
    };

    let errors = if object_type == "table" {
        current_metadata_integrity_errors(connection)?
    } else {
        vec![LithographError::storage(
            "Lithograph metadata has an invalid storage object type",
        )]
    };
    if let Some(error) = errors
        .iter()
        .find(|error| error.category == ErrorCategory::FormatTooNew)
    {
        return Err(error.clone());
    }
    let error_values: Vec<_> = errors
        .into_iter()
        .map(|error| error.to_json_value())
        .collect();

    Ok(json!({
        "ok": error_values.is_empty(),
        "errors": error_values,
        "checked": ["metadata"],
    }))
}

fn ensure_current_metadata_integrity(connection: &Connection) -> LithographResult<()> {
    if let Some(error) = current_metadata_integrity_errors(connection)?
        .into_iter()
        .next()
    {
        return Err(error);
    }
    Ok(())
}

fn current_metadata_integrity_errors(
    connection: &Connection,
) -> LithographResult<Vec<LithographError>> {
    let mut errors = Vec::new();

    let schema_sql = connection
        .query_row(
            "SELECT sql FROM main.sqlite_schema WHERE type = 'table' AND name = ?1",
            [META_TABLE],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata schema"))?
        .flatten();
    if !schema_sql
        .as_deref()
        .is_some_and(metadata_schema_sql_matches)
    {
        errors.push(LithographError::storage(
            "Lithograph metadata table schema does not match storage format 1",
        ));
    }

    let columns = metadata_columns(connection)?;
    let expected = [
        ("id", "INTEGER", 0, None, 1, 0),
        ("magic", "TEXT", 1, None, 0, 0),
        ("database_id", "TEXT", 1, None, 0, 0),
        ("storage_format", "INTEGER", 1, None, 0, 0),
    ];
    if columns.len() != expected.len()
        || columns.iter().zip(expected).any(|(actual, expected)| {
            actual.name != expected.0
                || actual.declared_type != expected.1
                || actual.not_null != expected.2
                || actual.default_value.as_deref() != expected.3
                || actual.primary_key != expected.4
                || actual.hidden != expected.5
        })
    {
        errors.push(LithographError::storage(
            "Lithograph metadata columns do not match storage format 1",
        ));
    }

    let row_count = connection
        .query_row("SELECT count(*) FROM main._lithograph_meta", [], |row| {
            row.get::<_, i64>(0)
        })
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata rows"))?;
    if row_count != 1 {
        errors.push(LithographError::storage(
            "Lithograph metadata must contain exactly one marker row",
        ));
    }

    let marker = connection
        .query_row(
            "SELECT magic, database_id, storage_format FROM main._lithograph_meta WHERE id = 1",
            [],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )
        .optional();

    match marker {
        Ok(Some((magic, database_id, storage_format))) => {
            if magic != MAGIC {
                errors.push(LithographError::storage(
                    "Lithograph metadata magic marker is invalid",
                ));
            }
            if !is_canonical_uuid(&database_id) {
                errors.push(LithographError::storage(
                    "Lithograph databaseId is not a canonical RFC 9562 UUID v4",
                ));
            }
            if storage_format > STORAGE_FORMAT_MAX {
                errors.push(format_too_new_error(storage_format));
            } else if storage_format < STORAGE_FORMAT_MIN {
                errors.push(LithographError::storage(format!(
                    "database storage format {storage_format} is below supported minimum {STORAGE_FORMAT_MIN}"
                )));
            }
        }
        Ok(None) => errors.push(LithographError::storage(
            "Lithograph metadata is missing its marker row",
        )),
        Err(error) => {
            let mapped = map_sqlite_error(error, "failed to inspect Lithograph metadata marker");
            if matches!(
                mapped.category,
                ErrorCategory::Busy | ErrorCategory::Resource | ErrorCategory::Io
            ) {
                return Err(mapped);
            }
            errors.push(LithographError::storage(
                "Lithograph metadata marker does not match storage format 1",
            ));
        }
    }

    Ok(errors)
}

fn metadata_columns(connection: &Connection) -> LithographResult<Vec<MetadataColumn>> {
    let mut statement = connection
        .prepare("PRAGMA main.table_xinfo('_lithograph_meta')")
        .map_err(|error| {
            map_sqlite_error(error, "failed to inspect Lithograph metadata columns")
        })?;
    let rows = statement
        .query_map([], |row| {
            Ok(MetadataColumn {
                name: row.get(1)?,
                declared_type: row.get(2)?,
                not_null: row.get(3)?,
                default_value: row.get(4)?,
                primary_key: row.get(5)?,
                hidden: row.get(6)?,
            })
        })
        .map_err(|error| {
            map_sqlite_error(error, "failed to inspect Lithograph metadata columns")
        })?;

    rows.collect::<SqliteResult<Vec<_>>>()
        .map_err(|error| map_sqlite_error(error, "failed to inspect Lithograph metadata columns"))
}

fn metadata_schema_sql_matches(sql: &str) -> bool {
    let normalized: String = sql
        .chars()
        .filter(|character| !character.is_ascii_whitespace())
        .flat_map(char::to_lowercase)
        .collect();
    normalized
        == "createtable_lithograph_meta(idintegerprimarykeycheck(id=1),magictextnotnull,database_idtextnotnull,storage_formatintegernotnull)"
        || normalized
            == "createtablemain._lithograph_meta(idintegerprimarykeycheck(id=1),magictextnotnull,database_idtextnotnull,storage_formatintegernotnull)"
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
        write!(&mut output, "{byte:02x}").expect("writing to String cannot fail");
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

#[repr(C)]
struct RowsTab {
    base: ffi::sqlite3_vtab,
    db: *mut ffi::sqlite3,
}

unsafe impl<'vtab> VTab<'vtab> for RowsTab {
    type Aux = ConnectionRegistration;
    type Cursor = RowsCursor<'vtab>;

    fn connect(
        db: &mut VTabConnection,
        _aux: Option<&Self::Aux>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        _args: &[&[u8]],
    ) -> SqliteResult<(Cow<'static, CStr>, Self)> {
        catch_sqlite_boundary(|| {
            db.config(VTabConfig::DirectOnly)?;
            let handle = unsafe { db.handle() };
            Ok((
                Cow::Borrowed(c"CREATE TABLE x(ordinal INTEGER, columns TEXT, row TEXT, query HIDDEN, params HIDDEN, options HIDDEN)"),
                Self {
                    base: ffi::sqlite3_vtab::default(),
                    db: handle,
                },
            ))
        })
    }

    fn best_index(&self, info: &mut IndexInfo) -> SqliteResult<bool> {
        catch_sqlite_boundary(|| {
            const QUERY_COLUMN: c_int = 3;
            const PARAMS_COLUMN: c_int = 4;
            const OPTIONS_COLUMN: c_int = 5;

            let mut selected = [None, None, None];
            for (index, constraint) in info.constraints().enumerate() {
                let slot = match constraint.column() {
                    QUERY_COLUMN => Some(0),
                    PARAMS_COLUMN => Some(1),
                    OPTIONS_COLUMN => Some(2),
                    _ => None,
                };
                let Some(slot) = slot else {
                    continue;
                };
                if constraint.is_usable()
                    && constraint.operator() == IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ
                {
                    selected[slot] = Some(index);
                }
            }

            let mut argv = 1;
            let mut idx_num = 0;
            for (slot, constraint_index) in selected.into_iter().enumerate() {
                if let Some(constraint_index) = constraint_index {
                    let mut usage = info.constraint_usage(constraint_index);
                    usage.set_argv_index(argv);
                    usage.set_omit(true);
                    argv += 1;
                    idx_num |= 1 << slot;
                }
            }
            info.set_idx_num(idx_num);
            if selected[0].is_some() {
                info.set_estimated_cost(10.0);
                info.set_estimated_rows(1000);
            } else {
                info.set_estimated_cost(1_000_000_000.0);
                info.set_estimated_rows(1);
            }
            Ok(true)
        })
    }

    fn open(&'vtab mut self) -> SqliteResult<Self::Cursor> {
        catch_sqlite_boundary(|| {
            Ok(RowsCursor {
                base: ffi::sqlite3_vtab_cursor::default(),
                db: self.db,
                exhausted: true,
                phantom: PhantomData,
            })
        })
    }
}

#[repr(C)]
struct RowsCursor<'vtab> {
    base: ffi::sqlite3_vtab_cursor,
    db: *mut ffi::sqlite3,
    exhausted: bool,
    phantom: PhantomData<&'vtab RowsTab>,
}

unsafe impl VTabCursor for RowsCursor<'_> {
    fn filter(
        &mut self,
        idx_num: c_int,
        _idx_str: Option<&str>,
        args: &Filters<'_>,
    ) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            let mut index = 0;
            let query = if idx_num & 1 != 0 {
                let value = args.get::<String>(index).map_err(|_| {
                    LithographError::invalid_argument("query must be TEXT").to_sqlite_error()
                })?;
                index += 1;
                value
            } else {
                return Err(
                    LithographError::invalid_argument("query is required").to_sqlite_error()
                );
            };
            let params = if idx_num & 2 != 0 {
                let value = args.get::<String>(index).map_err(|_| {
                    LithographError::invalid_argument("params must be JSON TEXT").to_sqlite_error()
                })?;
                index += 1;
                value
            } else {
                "{}".to_owned()
            };
            let options = if idx_num & 4 != 0 {
                args.get::<String>(index).map_err(|_| {
                    LithographError::invalid_argument("options must be JSON TEXT").to_sqlite_error()
                })?
            } else {
                "{}".to_owned()
            };

            if query.trim().is_empty() {
                return Err(
                    LithographError::invalid_argument("query must not be empty").to_sqlite_error()
                );
            }
            validate_json_object(&params, "params").map_err(|e| e.to_sqlite_error())?;
            validate_json_object(&options, "options").map_err(|e| e.to_sqlite_error())?;
            let connection = unsafe { Connection::from_handle(self.db) }.map_err(|error| {
                map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
            })?;
            require_initialized(&connection).map_err(|e| e.to_sqlite_error())?;

            self.exhausted = true;
            Err(LithographError::semantic_unavailable().to_sqlite_error())
        })
    }

    fn next(&mut self) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            self.exhausted = true;
            Ok(())
        })
    }

    fn eof(&self) -> bool {
        self.exhausted
    }

    fn column(&self, _ctx: &mut VTabContext, _i: c_int) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            Err(
                LithographError::internal("lithograph_rows cursor has no current row")
                    .to_sqlite_error(),
            )
        })
    }

    fn rowid(&self) -> SqliteResult<i64> {
        catch_sqlite_boundary(|| {
            Err(
                LithographError::internal("lithograph_rows cursor has no current row")
                    .to_sqlite_error(),
            )
        })
    }
}

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
    clear_error_out(error_json);
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        native_execute_impl(
            db,
            query,
            query_len,
            params_json,
            params_len,
            options_json,
            options_len,
            callback,
        )
    })) {
        Ok(Ok(())) => ffi::SQLITE_OK,
        Ok(Err(error)) => unsafe { write_native_error(error_json, &error) },
        Err(_) => unsafe {
            write_native_error(
                error_json,
                &LithographError::internal("panic while executing native ABI call"),
            )
        },
    }
}

#[allow(clippy::too_many_arguments)]
unsafe fn native_execute_impl(
    db: *mut ffi::sqlite3,
    query: *const c_char,
    query_len: usize,
    params_json: *const c_char,
    params_len: usize,
    options_json: *const c_char,
    options_len: usize,
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

    let query = unsafe { input_utf8(query, query_len, "query")? };
    let params = unsafe { input_utf8(params_json, params_len, "params_json")? };
    let options = unsafe { input_utf8(options_json, options_len, "options_json")? };
    if query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty"));
    }
    validate_json_object(&params, "params")?;
    validate_json_object(&options, "options")?;
    let connection = unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "invalid SQLite connection"))?;
    require_initialized(&connection)?;

    Err(LithographError::semantic_unavailable())
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
    clear_error_out(error_json);
    match catch_unwind(AssertUnwindSafe(|| unsafe {
        native_validate_impl(db, query, query_len)
    })) {
        Ok(Ok(())) => ffi::SQLITE_OK,
        Ok(Err(error)) => unsafe { write_native_error(error_json, &error) },
        Err(_) => unsafe {
            write_native_error(
                error_json,
                &LithographError::internal("panic while executing native ABI call"),
            )
        },
    }
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
    let query = unsafe { input_utf8(query, query_len, "query")? };
    if query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty"));
    }
    let connection = unsafe { Connection::from_handle(db) }
        .map_err(|error| map_sqlite_error(error, "invalid SQLite connection"))?;
    require_initialized(&connection)?;
    Err(LithographError::semantic_unavailable())
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

unsafe fn input_utf8(
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
    let bytes = unsafe { std::slice::from_raw_parts(pointer.cast::<u8>(), length) };
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| LithographError::invalid_argument(format!("{name} must contain valid UTF-8")))
}

fn clear_error_out(error_json: *mut *mut c_char) {
    if !error_json.is_null() {
        unsafe { *error_json = ptr::null_mut() };
    }
}

unsafe fn write_native_error(error_json: *mut *mut c_char, error: &LithographError) -> c_int {
    if !error_json.is_null()
        && let Ok(payload) = CString::new(error.to_json())
    {
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
        unsafe {
            drop(CString::from_raw(pointer.cast::<c_char>()));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn uuid_validation_accepts_rfc9562_v4_shape() {
        assert!(is_canonical_uuid("550e8400-e29b-41d4-a716-446655440000"));
        assert!(!is_canonical_uuid("550e8400-e29b-11d4-a716-446655440000"));
        assert!(!is_canonical_uuid("550E8400-E29B-41D4-A716-446655440000"));
    }

    #[test]
    fn error_json_has_stable_shape() {
        let value: Value = serde_json::from_str(&LithographError::not_initialized().to_json())
            .expect("error JSON must be valid");
        assert_eq!(value["category"], "NOT_INITIALIZED");
        assert_eq!(value["sqliteCode"], ffi::SQLITE_ERROR);
        assert!(value["line"].is_null());
        assert!(value["column"].is_null());
    }

    #[test]
    fn json_adapter_requires_object_inputs() {
        assert!(validate_json_object("{}", "params").is_ok());
        assert_eq!(
            validate_json_object("[]", "params")
                .expect_err("array must be rejected")
                .category,
            ErrorCategory::InvalidArgument
        );
    }

    #[test]
    fn sqlite_error_mapping_preserves_stable_primary_categories() {
        let cases = [
            (ffi::SQLITE_BUSY, ErrorCategory::Busy),
            (ffi::SQLITE_LOCKED, ErrorCategory::Busy),
            (ffi::SQLITE_NOMEM, ErrorCategory::Resource),
            (ffi::SQLITE_TOOBIG, ErrorCategory::Resource),
            (ffi::SQLITE_FULL, ErrorCategory::Resource),
            (ffi::SQLITE_IOERR, ErrorCategory::Io),
            (ffi::SQLITE_CANTOPEN, ErrorCategory::Io),
            (ffi::SQLITE_READONLY, ErrorCategory::Io),
            (ffi::SQLITE_ERROR, ErrorCategory::Storage),
        ];
        for (code, expected) in cases {
            let error = SqliteError::SqliteFailure(ffi::Error::new(code), None);
            assert_eq!(map_sqlite_error(error, "probe").category, expected);
        }
    }

    #[test]
    fn native_input_utf8_copies_and_validates_input() {
        let bytes = b"RETURN 1";
        let value = unsafe {
            input_utf8(bytes.as_ptr().cast::<c_char>(), bytes.len(), "query")
                .expect("valid UTF-8 must be accepted")
        };
        assert_eq!(value, "RETURN 1");

        let invalid = [0xff_u8];
        let error = unsafe {
            input_utf8(invalid.as_ptr().cast::<c_char>(), invalid.len(), "query")
                .expect_err("invalid UTF-8 must be rejected")
        };
        assert_eq!(error.category, ErrorCategory::InvalidArgument);

        let error = unsafe {
            input_utf8(ptr::null(), 1, "query").expect_err("NULL plus length must be misuse")
        };
        assert_eq!(error.sqlite_code, ffi::SQLITE_MISUSE);
    }

    #[test]
    fn virtual_table_panic_guard_converts_panics_to_internal_errors() {
        let error = catch_sqlite_boundary::<()>(|| panic!("boundary probe"))
            .expect_err("panic must become a SQLite error");
        match error {
            SqliteError::SqliteFailure(_, Some(message)) => {
                assert!(message.contains("LITHOGRAPH_INTERNAL_ERROR"));
            }
            other => panic!("unexpected panic guard result: {other:?}"),
        }
    }
}
