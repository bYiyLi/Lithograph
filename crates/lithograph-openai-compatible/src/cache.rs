use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::types::Value as SqlValue;
use rusqlite::{Connection, Error as SqliteError, OpenFlags, OptionalExtension as _, params};

use crate::PROVIDER_SEMANTIC_IDENTITY;
use crate::config::ProviderConfig;
use crate::http::{Client, Failure, FailureKind, environment_value};

const CACHE_MAGIC: &str = "lithograph-openai-compatible-cache";
const CACHE_SCHEMA_VERSION: i64 = 1;
const CACHE_KEY_ENCODING_VERSION: &[u8] = b"LITHOGRAPH_OPENAI_CACHE_KEY_V1";
const BUSY_RETRY_BUDGET: Duration = Duration::from_millis(500);
const BUSY_RETRY_STEP: Duration = Duration::from_millis(10);
const META_TABLE: &str = "_openai_compatible_cache_meta";
const ENTRY_TABLE: &str = "_openai_compatible_embedding_entries";

struct CacheRequest {
    headers: Vec<(String, String)>,
    space_hash: [u8; 32],
    cache: Cache,
}

struct CacheLookup {
    resolved: BTreeMap<String, Vec<f32>>,
    misses: Vec<String>,
}

pub(crate) fn embed(
    client: &Client,
    config: &ProviderConfig,
    main_database: Option<&Path>,
    texts: &[String],
    dimensions: usize,
    mut is_cancelled: impl FnMut() -> bool,
) -> Result<Vec<f32>, Failure> {
    let CacheRequest {
        headers,
        space_hash,
        cache,
    } = open_cache_request(config, main_database, dimensions, &mut is_cancelled)?;
    let CacheLookup {
        mut resolved,
        misses,
    } = lookup_cached_texts(&cache, &space_hash, texts, dimensions, &mut is_cancelled)?;
    resolve_remote_misses(
        client,
        config,
        &cache,
        &headers,
        &space_hash,
        dimensions,
        misses,
        &mut resolved,
        &mut is_cancelled,
    )?;
    assemble_output(texts, dimensions, &resolved)
}

fn open_cache_request(
    config: &ProviderConfig,
    main_database: Option<&Path>,
    dimensions: usize,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<CacheRequest, Failure> {
    let headers = config
        .final_headers(environment_value)
        .map_err(Failure::invalid)?;
    let space_hash = cache_space_hash(config, dimensions, &headers);
    let path = config
        .cache
        .path
        .as_deref()
        .ok_or_else(|| Failure::invalid("cache.path is required when cache.enabled=true"))?;
    let cache = Cache::open(
        Path::new(path),
        main_database,
        config.cache.max_bytes,
        is_cancelled,
    )?;
    Ok(CacheRequest {
        headers,
        space_hash,
        cache,
    })
}

fn lookup_cached_texts(
    cache: &Cache,
    space_hash: &[u8; 32],
    texts: &[String],
    dimensions: usize,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<CacheLookup, Failure> {
    let mut resolved = BTreeMap::<String, Vec<f32>>::new();
    let mut misses = Vec::new();
    for text in texts {
        ensure_not_cancelled(is_cancelled)?;
        if resolved.contains_key(text) || misses.iter().any(|candidate| candidate == text) {
            continue;
        }
        match cache.lookup(space_hash, text, dimensions, is_cancelled)? {
            Some(vector) => {
                resolved.insert(text.clone(), vector);
            }
            None => misses.push(text.clone()),
        }
    }
    Ok(CacheLookup { resolved, misses })
}

#[allow(
    clippy::too_many_arguments,
    reason = "remote miss resolution keeps provider/cache request identity explicit"
)]
fn resolve_remote_misses(
    client: &Client,
    config: &ProviderConfig,
    cache: &Cache,
    headers: &[(String, String)],
    space_hash: &[u8; 32],
    dimensions: usize,
    misses: Vec<String>,
    resolved: &mut BTreeMap<String, Vec<f32>>,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<(), Failure> {
    if misses.is_empty() {
        return Ok(());
    }
    let values =
        client.embed_with_headers(config, &misses, dimensions, headers, &mut *is_cancelled)?;
    let generated = misses
        .iter()
        .zip(values.chunks_exact(dimensions))
        .map(|(text, vector)| (text.clone(), vector.to_vec()))
        .collect::<Vec<_>>();
    cache.publish(space_hash, dimensions, &generated, is_cancelled)?;
    resolved.extend(generated);
    Ok(())
}

fn assemble_output(
    texts: &[String],
    dimensions: usize,
    resolved: &BTreeMap<String, Vec<f32>>,
) -> Result<Vec<f32>, Failure> {
    let value_count = texts
        .len()
        .checked_mul(dimensions)
        .ok_or_else(|| Failure::resource("embedding output size overflow"))?;
    let mut output = Vec::with_capacity(value_count);
    for text in texts {
        let vector = resolved
            .get(text)
            .ok_or_else(|| Failure::internal("embedding cache merge lost an input"))?;
        output.extend_from_slice(vector);
    }
    Ok(output)
}

struct Cache {
    connection: Connection,
    max_bytes: u64,
}

impl Cache {
    fn open(
        path: &Path,
        main_database: Option<&Path>,
        max_bytes: u64,
        is_cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Self, Failure> {
        ensure_not_cancelled(is_cancelled)?;
        let connection = Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX,
        )
        .map_err(map_sqlite_error)?;
        connection
            .busy_timeout(Duration::ZERO)
            .map_err(map_sqlite_error)?;
        if main_database.is_some_and(|main| same_underlying_file(main, path)) {
            return Err(Failure::invalid(
                "cache.path must not refer to the host main database",
            ));
        }
        let cache = Self {
            connection,
            max_bytes,
        };
        cache.initialize_or_validate(is_cancelled)?;
        cache.enforce_budget(is_cancelled)?;
        Ok(cache)
    }

    fn initialize_or_validate(
        &self,
        is_cancelled: &mut impl FnMut() -> bool,
    ) -> Result<(), Failure> {
        let objects = retry_sqlite(is_cancelled, || self.object_names())?;
        if objects.is_empty() {
            return self.initialize(is_cancelled);
        }
        let mut expected = vec![ENTRY_TABLE.to_owned(), META_TABLE.to_owned()];
        expected.sort();
        if objects != expected {
            return Err(Failure::invalid(
                "cache database is not an openai-compatible Provider cache",
            ));
        }
        let marker = retry_sqlite(is_cancelled, || {
            self.connection.query_row(
                &format!(
                    "SELECT magic,schema_version,used_payload_bytes,used_payload_bytes_check FROM {META_TABLE} WHERE id=1"
                ),
                [],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
        })
        .map_err(cache_structure_error)?;
        if marker.0 != CACHE_MAGIC || marker.1 != CACHE_SCHEMA_VERSION {
            return Err(Failure::invalid(
                "cache database marker or schema version is incompatible",
            ));
        }
        validate_used_counter(marker.2, marker.3)?;
        Ok(())
    }

    fn initialize(&self, is_cancelled: &mut impl FnMut() -> bool) -> Result<(), Failure> {
        with_immediate_transaction(&self.connection, is_cancelled, |_is_cancelled| {
            self.connection.execute_batch(&format!(
                "CREATE TABLE {META_TABLE}(\
                    id INTEGER PRIMARY KEY CHECK(id=1),\
                    magic TEXT NOT NULL,\
                    schema_version INTEGER NOT NULL,\
                    used_payload_bytes INTEGER NOT NULL CHECK(used_payload_bytes>=0),\
                    used_payload_bytes_check INTEGER NOT NULL,\
                    CHECK(used_payload_bytes_check = ~used_payload_bytes)\
                 );\
                 INSERT INTO {META_TABLE}(id,magic,schema_version,used_payload_bytes,used_payload_bytes_check)\
                 VALUES(1,'{CACHE_MAGIC}',{CACHE_SCHEMA_VERSION},0,-1);\
                 CREATE TABLE {ENTRY_TABLE}(\
                    entry_id INTEGER PRIMARY KEY AUTOINCREMENT,\
                    space_hash BLOB NOT NULL CHECK(length(space_hash)=32),\
                    text_hash BLOB NOT NULL CHECK(length(text_hash)=32),\
                    text_bytes INTEGER NOT NULL CHECK(text_bytes>=0),\
                    dimension INTEGER NOT NULL CHECK(dimension BETWEEN 1 AND 4096),\
                    vector_blob BLOB NOT NULL,\
                    payload_bytes INTEGER NOT NULL CHECK(payload_bytes>=0),\
                    UNIQUE(space_hash,text_hash)\
                 );"
            ))?;
            Ok(())
        })
    }

    fn object_names(&self) -> rusqlite::Result<Vec<String>> {
        let mut statement = self.connection.prepare(
            "SELECT name FROM sqlite_schema WHERE type='table' AND name NOT LIKE 'sqlite_%' ORDER BY name",
        )?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        rows.collect()
    }

    fn lookup(
        &self,
        space_hash: &[u8; 32],
        text: &str,
        dimensions: usize,
        is_cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Option<Vec<f32>>, Failure> {
        let text_hash = *blake3::hash(text.as_bytes()).as_bytes();
        let row = retry_sqlite(is_cancelled, || {
            self.connection
                .query_row(
                    &format!(
                        "SELECT entry_id,text_bytes,dimension,vector_blob,payload_bytes FROM {ENTRY_TABLE} WHERE space_hash=?1 AND text_hash=?2"
                    ),
                    params![space_hash.as_slice(), text_hash.as_slice()],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, SqlValue>(1)?,
                            row.get::<_, SqlValue>(2)?,
                            row.get::<_, SqlValue>(3)?,
                            row.get::<_, SqlValue>(4)?,
                        ))
                    },
                )
                .optional()
        })?;
        let Some((entry_id, text_bytes, dimension, vector_blob, payload_bytes)) = row else {
            return Ok(None);
        };

        let payload = match payload_bytes {
            SqlValue::Integer(value) if value >= 0 => u64::try_from(value)
                .map_err(|_| Failure::io("cache entry payload accounting is invalid"))?,
            _ => {
                return Err(Failure::io(
                    "cache entry payload accounting is structurally invalid",
                ));
            }
        };
        let vector = validate_entry(
            text,
            dimensions,
            text_bytes,
            dimension,
            vector_blob,
            payload,
        );
        match vector {
            Ok(vector) => Ok(Some(vector)),
            Err(EntryDamage::Recoverable) => {
                self.delete_corrupt_entry(entry_id, payload, is_cancelled)?;
                Ok(None)
            }
            Err(EntryDamage::Structural) => Err(Failure::io(
                "cache entry is corrupt and cannot be repaired safely",
            )),
        }
    }

    fn delete_corrupt_entry(
        &self,
        entry_id: i64,
        payload: u64,
        is_cancelled: &mut impl FnMut() -> bool,
    ) -> Result<(), Failure> {
        with_immediate_transaction(&self.connection, is_cancelled, |_is_cancelled| {
            let (used, check) = read_used_counter(&self.connection)?;
            if used < 0 || check != !used {
                return Err(SqliteError::InvalidQuery);
            }
            let mut used = u64::try_from(used).map_err(|_| SqliteError::InvalidQuery)?;
            let changed = self.connection.execute(
                &format!("DELETE FROM {ENTRY_TABLE} WHERE entry_id=?1"),
                [entry_id],
            )?;
            if changed == 0 {
                return Ok(());
            }
            used = used
                .checked_sub(payload)
                .ok_or_else(|| SqliteError::InvalidQuery)?;
            write_used_counter(&self.connection, used)?;
            Ok(())
        })
        .map_err(|failure| match failure.kind {
            FailureKind::Internal => Failure::io("cache accounting is inconsistent"),
            _ => failure,
        })
    }

    fn publish(
        &self,
        space_hash: &[u8; 32],
        dimensions: usize,
        generated: &[(String, Vec<f32>)],
        is_cancelled: &mut impl FnMut() -> bool,
    ) -> Result<(), Failure> {
        with_immediate_transaction(&self.connection, is_cancelled, |is_cancelled| {
            let mut used = read_valid_used_counter(&self.connection)?;
            used = self.publish_entries(space_hash, dimensions, generated, used, is_cancelled)?;
            used = evict_to_budget(&self.connection, used, self.max_bytes)?;
            write_used_counter(&self.connection, used)?;
            Ok(())
        })
    }

    fn publish_entries(
        &self,
        space_hash: &[u8; 32],
        dimensions: usize,
        generated: &[(String, Vec<f32>)],
        mut used: u64,
        is_cancelled: &mut (impl FnMut() -> bool + ?Sized),
    ) -> Result<u64, SqliteError> {
        for (text, vector) in generated {
            if is_cancelled() {
                return Err(interrupted_sqlite_error());
            }
            if let Some(payload) = self.publish_entry(space_hash, dimensions, text, vector)? {
                used = used.checked_add(payload).ok_or(SqliteError::InvalidQuery)?;
            }
        }
        Ok(used)
    }

    fn publish_entry(
        &self,
        space_hash: &[u8; 32],
        dimensions: usize,
        text: &str,
        vector: &[f32],
    ) -> Result<Option<u64>, SqliteError> {
        let payload =
            u64::try_from(vector.len().saturating_mul(4)).map_err(|_| SqliteError::InvalidQuery)?;
        if payload > self.max_bytes {
            return Ok(None);
        }
        let blob = encode_vector(vector);
        let text_hash = *blake3::hash(text.as_bytes()).as_bytes();
        let text_bytes = i64::try_from(text.len()).map_err(|_| SqliteError::InvalidQuery)?;
        let dimension = i64::try_from(dimensions).map_err(|_| SqliteError::InvalidQuery)?;
        let payload_i64 = i64::try_from(payload).map_err(|_| SqliteError::InvalidQuery)?;
        let changed = self.connection.execute(
            &format!(
                "INSERT OR IGNORE INTO {ENTRY_TABLE}(\
                 space_hash,text_hash,text_bytes,dimension,vector_blob,payload_bytes)\
                 VALUES(?1,?2,?3,?4,?5,?6)"
            ),
            params![
                space_hash.as_slice(),
                text_hash.as_slice(),
                text_bytes,
                dimension,
                blob,
                payload_i64
            ],
        )?;
        Ok((changed != 0).then_some(payload))
    }

    fn enforce_budget(&self, is_cancelled: &mut impl FnMut() -> bool) -> Result<(), Failure> {
        let (used, check) = retry_sqlite(is_cancelled, || read_used_counter(&self.connection))?;
        validate_used_counter_i64(used, check)?;
        let used =
            u64::try_from(used).map_err(|_| Failure::io("cache payload accounting is invalid"))?;
        if used <= self.max_bytes {
            return Ok(());
        }
        with_immediate_transaction(&self.connection, is_cancelled, |_is_cancelled| {
            let (used, check) = read_used_counter(&self.connection)?;
            if used < 0 || check != !used {
                return Err(SqliteError::InvalidQuery);
            }
            let used = u64::try_from(used).map_err(|_| SqliteError::InvalidQuery)?;
            let used = evict_to_budget(&self.connection, used, self.max_bytes)?;
            write_used_counter(&self.connection, used)?;
            Ok(())
        })
    }
}

fn cache_space_hash(
    config: &ProviderConfig,
    dimensions: usize,
    headers: &[(String, String)],
) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hash_field(&mut hasher, CACHE_KEY_ENCODING_VERSION);
    hash_field(&mut hasher, PROVIDER_SEMANTIC_IDENTITY);
    hash_field(&mut hasher, config.embeddings_url().as_bytes());
    hash_field(&mut hasher, config.model.as_bytes());
    hash_field(&mut hasher, &(dimensions as u64).to_le_bytes());
    hash_field(&mut hasher, &[u8::from(config.send_dimensions)]);
    hash_field(&mut hasher, config.encoding_format.as_str().as_bytes());
    hash_optional(&mut hasher, config.user.as_deref());
    hash_optional(&mut hasher, config.semantic_identity.as_deref());
    for (name, value) in headers {
        hash_field(&mut hasher, name.to_ascii_lowercase().as_bytes());
        let digest = blake3::hash(value.as_bytes());
        hash_field(&mut hasher, digest.as_bytes());
    }
    *hasher.finalize().as_bytes()
}

fn hash_field(hasher: &mut blake3::Hasher, value: &[u8]) {
    hasher.update(&(value.len() as u64).to_le_bytes());
    hasher.update(value);
}

fn hash_optional(hasher: &mut blake3::Hasher, value: Option<&str>) {
    match value {
        Some(value) => {
            hasher.update(&[1]);
            hash_field(hasher, value.as_bytes());
        }
        None => {
            hasher.update(&[0]);
        }
    }
}

fn encode_vector(vector: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(vector.len().saturating_mul(4));
    for value in vector {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

enum EntryDamage {
    Recoverable,
    Structural,
}

fn validate_entry(
    text: &str,
    dimensions: usize,
    text_bytes: SqlValue,
    dimension: SqlValue,
    vector_blob: SqlValue,
    payload: u64,
) -> Result<Vec<f32>, EntryDamage> {
    let expected_text = i64::try_from(text.len()).map_err(|_| EntryDamage::Structural)?;
    if !matches!(text_bytes, SqlValue::Integer(value) if value == expected_text) {
        return Err(EntryDamage::Recoverable);
    }
    let expected_dimension = i64::try_from(dimensions).map_err(|_| EntryDamage::Structural)?;
    if !matches!(dimension, SqlValue::Integer(value) if value == expected_dimension) {
        return Err(EntryDamage::Recoverable);
    }
    let expected_payload = dimensions.checked_mul(4).ok_or(EntryDamage::Structural)?;
    if payload != expected_payload as u64 {
        return Err(EntryDamage::Recoverable);
    }
    let SqlValue::Blob(blob) = vector_blob else {
        return Err(EntryDamage::Recoverable);
    };
    if blob.len() != expected_payload {
        return Err(EntryDamage::Recoverable);
    }
    let mut vector = Vec::with_capacity(dimensions);
    for chunk in blob.as_chunks::<4>().0 {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return Err(EntryDamage::Recoverable);
        }
        vector.push(value);
    }
    Ok(vector)
}

fn read_used_counter(connection: &Connection) -> rusqlite::Result<(i64, i64)> {
    connection.query_row(
        &format!("SELECT used_payload_bytes,used_payload_bytes_check FROM {META_TABLE} WHERE id=1"),
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )
}

fn read_valid_used_counter(connection: &Connection) -> rusqlite::Result<u64> {
    let (used, check) = read_used_counter(connection)?;
    if used < 0 || check != !used {
        return Err(SqliteError::InvalidQuery);
    }
    u64::try_from(used).map_err(|_| SqliteError::InvalidQuery)
}

fn interrupted_sqlite_error() -> SqliteError {
    SqliteError::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_INTERRUPT),
        None,
    )
}

fn validate_used_counter(used: i64, check: i64) -> Result<(), Failure> {
    validate_used_counter_i64(used, check)
}

fn validate_used_counter_i64(used: i64, check: i64) -> Result<(), Failure> {
    if used < 0 || check != !used {
        return Err(Failure::io("cache payload accounting metadata is corrupt"));
    }
    Ok(())
}

fn write_used_counter(connection: &Connection, used: u64) -> rusqlite::Result<()> {
    let used = i64::try_from(used).map_err(|_| SqliteError::InvalidQuery)?;
    connection.execute(
        &format!(
            "UPDATE {META_TABLE} SET used_payload_bytes=?1,used_payload_bytes_check=?2 WHERE id=1"
        ),
        params![used, !used],
    )?;
    Ok(())
}

fn evict_to_budget(
    connection: &Connection,
    mut used: u64,
    max_bytes: u64,
) -> rusqlite::Result<u64> {
    while used > max_bytes {
        let oldest = connection
            .query_row(
                &format!(
                    "SELECT entry_id,payload_bytes FROM {ENTRY_TABLE} ORDER BY entry_id LIMIT 1"
                ),
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .optional()?
            .ok_or(SqliteError::InvalidQuery)?;
        if oldest.1 < 0 {
            return Err(SqliteError::InvalidQuery);
        }
        let payload = u64::try_from(oldest.1).map_err(|_| SqliteError::InvalidQuery)?;
        used = used.checked_sub(payload).ok_or(SqliteError::InvalidQuery)?;
        let changed = connection.execute(
            &format!("DELETE FROM {ENTRY_TABLE} WHERE entry_id=?1"),
            [oldest.0],
        )?;
        if changed != 1 {
            return Err(SqliteError::InvalidQuery);
        }
    }
    Ok(used)
}

fn with_immediate_transaction(
    connection: &Connection,
    is_cancelled: &mut impl FnMut() -> bool,
    operation: impl FnOnce(&mut dyn FnMut() -> bool) -> rusqlite::Result<()>,
) -> Result<(), Failure> {
    retry_sqlite(is_cancelled, || connection.execute_batch("BEGIN IMMEDIATE"))?;
    let result = operation(is_cancelled);
    match result {
        Ok(()) => {
            if let Err(error) = retry_sqlite(is_cancelled, || connection.execute_batch("COMMIT")) {
                let _ = connection.execute_batch("ROLLBACK");
                return Err(error);
            }
            Ok(())
        }
        Err(error) => {
            let _ = connection.execute_batch("ROLLBACK");
            Err(map_sqlite_error(error))
        }
    }
}

fn retry_sqlite<T>(
    is_cancelled: &mut impl FnMut() -> bool,
    mut operation: impl FnMut() -> rusqlite::Result<T>,
) -> Result<T, Failure> {
    let started = Instant::now();
    loop {
        ensure_not_cancelled(is_cancelled)?;
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) if is_busy(&error) && started.elapsed() < BUSY_RETRY_BUDGET => {
                thread::sleep(BUSY_RETRY_STEP);
            }
            Err(error) => return Err(map_sqlite_error(error)),
        }
    }
}

fn ensure_not_cancelled(is_cancelled: &mut impl FnMut() -> bool) -> Result<(), Failure> {
    if is_cancelled() {
        Err(Failure::cancelled())
    } else {
        Ok(())
    }
}

fn is_busy(error: &SqliteError) -> bool {
    matches!(
        error,
        SqliteError::SqliteFailure(code, _)
            if matches!(
                code.extended_code & 0xff,
                rusqlite::ffi::SQLITE_BUSY | rusqlite::ffi::SQLITE_LOCKED
            )
    )
}

fn map_sqlite_error(error: SqliteError) -> Failure {
    if let SqliteError::SqliteFailure(code, _) = &error {
        return match code.extended_code & 0xff {
            rusqlite::ffi::SQLITE_INTERRUPT => Failure::cancelled(),
            rusqlite::ffi::SQLITE_NOMEM
            | rusqlite::ffi::SQLITE_FULL
            | rusqlite::ffi::SQLITE_TOOBIG => {
                Failure::resource("cache database exhausted a required resource")
            }
            _ => Failure::io("cache database operation failed"),
        };
    }
    Failure::io("cache database operation failed")
}

fn cache_structure_error(failure: Failure) -> Failure {
    if failure.kind == FailureKind::Cancelled {
        failure
    } else {
        Failure::io("cache database structure is invalid")
    }
}

fn same_underlying_file(left: &Path, right: &Path) -> bool {
    let left_canonical = fs::canonicalize(left).ok();
    let right_canonical = fs::canonicalize(right).ok();
    if left_canonical.is_some() && left_canonical == right_canonical {
        return true;
    }
    same_file_metadata(left, right)
}

#[cfg(unix)]
fn same_file_metadata(left: &Path, right: &Path) -> bool {
    use std::os::unix::fs::MetadataExt as _;
    let Ok(left) = fs::metadata(left) else {
        return false;
    };
    let Ok(right) = fs::metadata(right) else {
        return false;
    };
    left.dev() == right.dev() && left.ino() == right.ino()
}

#[cfg(not(unix))]
fn same_file_metadata(_left: &Path, _right: &Path) -> bool {
    false
}

pub(crate) fn main_database_path(connection: &Connection) -> Option<PathBuf> {
    connection
        .path()
        .filter(|path| !path.is_empty())
        .map(PathBuf::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cache_space_excludes_operational_cache_and_http_policy() {
        let base = ProviderConfig::parse(
            br#"{"model":"m","api_key":"a","timeout_ms":1,"max_retries":0,"batch_size":1}"#,
        )
        .expect("base");
        let changed = ProviderConfig::parse(
            br#"{"model":"m","api_key":"a","timeout_ms":999,"max_retries":8,"batch_size":20,"cache":{"enabled":false,"path":"ignored.db","max_bytes":1}}"#,
        )
        .expect("changed");
        let headers = base.final_headers(|_| Ok(None)).expect("headers");
        let changed_headers = changed.final_headers(|_| Ok(None)).expect("headers");
        assert_eq!(
            cache_space_hash(&base, 3, &headers),
            cache_space_hash(&changed, 3, &changed_headers)
        );
    }

    #[test]
    fn request_semantics_and_secrets_change_cache_space_without_exposing_secret() {
        let left = ProviderConfig::parse(br#"{"model":"m","api_key":"secret-a"}"#).expect("left");
        let right = ProviderConfig::parse(br#"{"model":"m","api_key":"secret-b"}"#).expect("right");
        let left_headers = left.final_headers(|_| Ok(None)).expect("headers");
        let right_headers = right.final_headers(|_| Ok(None)).expect("headers");
        assert_ne!(
            cache_space_hash(&left, 3, &left_headers),
            cache_space_hash(&right, 3, &right_headers)
        );
        let digest = cache_space_hash(&left, 3, &left_headers);
        assert!(!hex_bytes(&digest).contains("secret"));
    }

    fn hex_bytes(bytes: &[u8]) -> String {
        bytes.iter().map(|byte| format!("{byte:02x}")).collect()
    }
}
