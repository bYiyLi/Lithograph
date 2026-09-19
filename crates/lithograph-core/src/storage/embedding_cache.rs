use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension as _, params};
use serde_json::Value as JsonValue;

use super::encoding::{RecordHasher, u64_bytes};
use super::{HashId, StorageError, StorageResult};

const EMBEDDING_CACHE_FORMAT: i64 = 4;
const EMBEDDING_CACHE_ENCODING_VERSION: u64 = 1;
const FLOAT32_COORDINATE_TYPE: i64 = 5;
const DEFAULT_MAX_BYTES: u64 = 1_073_741_824;
const QUERY_CACHE_MAX_ENTRIES: i64 = 2_048;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingCachePolicy {
    pub enabled: bool,
    pub max_bytes: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingCacheStats {
    pub policy: EmbeddingCachePolicy,
    pub used_bytes: u64,
    pub entries: u64,
    pub spaces: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct EmbeddingCacheClear {
    pub deleted_entries: u64,
    pub released_payload_bytes: u64,
}

#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddingCacheEntry {
    pub text: String,
    pub vector: Vec<f32>,
}

struct CachedEmbeddingRow {
    text_bytes: i64,
    dimension: i64,
    coordinate_type: i64,
    vector_blob: Vec<u8>,
    payload_bytes: i64,
}

pub fn require_embedding_cache_format(connection: &Connection) -> StorageResult<()> {
    let format = storage_format(connection)?;
    if format != EMBEDDING_CACHE_FORMAT {
        return Err(StorageError::corrupt(format!(
            "Managed Semantic requires storage format {EMBEDDING_CACHE_FORMAT}; database is format {format}; run lithograph_init()"
        )));
    }
    Ok(())
}

pub fn embedding_cache_policy(connection: &Connection) -> StorageResult<EmbeddingCachePolicy> {
    require_embedding_cache_format(connection)?;
    let (enabled, max_bytes): (Option<i64>, Option<i64>) = connection.query_row(
        r#"SELECT "semantic.embedding_cache.enabled", "semantic.embedding_cache.max_bytes"
           FROM main._lithograph_meta WHERE id = 1"#,
        [],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    let enabled = match enabled {
        None | Some(1) => true,
        Some(0) => false,
        Some(_) => {
            return Err(StorageError::corrupt(
                "semantic embedding cache enabled metadata is invalid",
            ));
        }
    };
    let max_bytes = match max_bytes {
        None => DEFAULT_MAX_BYTES,
        Some(value) if value > 0 => u64::try_from(value).map_err(|_| {
            StorageError::corrupt("semantic embedding cache max_bytes is outside supported range")
        })?,
        Some(_) => {
            return Err(StorageError::corrupt(
                "semantic embedding cache max_bytes must be positive",
            ));
        }
    };
    Ok(EmbeddingCachePolicy { enabled, max_bytes })
}

pub fn embedding_cache_configure(
    connection: &Connection,
    enabled: Option<bool>,
    max_bytes: Option<u64>,
) -> StorageResult<EmbeddingCachePolicy> {
    require_embedding_cache_format(connection)?;
    if max_bytes == Some(0) || max_bytes.is_some_and(|value| value > i64::MAX as u64) {
        return Err(StorageError::corrupt(
            "semantic embedding cache max_bytes must be a positive INTEGER64",
        ));
    }
    with_savepoint(connection, "lithograph_embedding_cache_configure", || {
        if let Some(enabled) = enabled {
            connection.execute(
                r#"UPDATE main._lithograph_meta
                   SET "semantic.embedding_cache.enabled" = ?1 WHERE id = 1"#,
                [i64::from(enabled)],
            )?;
        }
        if let Some(max_bytes) = max_bytes {
            connection.execute(
                r#"UPDATE main._lithograph_meta
                   SET "semantic.embedding_cache.max_bytes" = ?1 WHERE id = 1"#,
                [i64::try_from(max_bytes).map_err(|_| {
                    StorageError::corrupt("semantic embedding cache max_bytes is too large")
                })?],
            )?;
        }
        let policy = embedding_cache_policy(connection)?;
        if policy.enabled {
            evict_to_budget(connection, policy.max_bytes)?;
        }
        Ok(policy)
    })
}

pub fn embedding_cache_stats(connection: &Connection) -> StorageResult<EmbeddingCacheStats> {
    let policy = embedding_cache_policy(connection)?;
    let (used, entries, spaces): (i64, i64, i64) = connection.query_row(
        "SELECT COALESCE(sum(payload_bytes), 0), count(*), count(DISTINCT hex(space_hash))          FROM main._lithograph_embedding_cache",
        [],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
    )?;
    Ok(EmbeddingCacheStats {
        policy,
        used_bytes: nonnegative_u64(used, "embedding cache used bytes")?,
        entries: nonnegative_u64(entries, "embedding cache entry count")?,
        spaces: nonnegative_u64(spaces, "embedding cache space count")?,
    })
}

pub fn embedding_cache_clear(connection: &Connection) -> StorageResult<EmbeddingCacheClear> {
    require_embedding_cache_format(connection)?;
    with_savepoint(connection, "lithograph_embedding_cache_clear", || {
        let (entries, bytes): (i64, i64) = connection.query_row(
            "SELECT count(*), COALESCE(sum(payload_bytes), 0) FROM main._lithograph_embedding_cache",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )?;
        connection.execute("DELETE FROM main._lithograph_embedding_cache", [])?;
        Ok(EmbeddingCacheClear {
            deleted_entries: nonnegative_u64(entries, "embedding cache entry count")?,
            released_payload_bytes: nonnegative_u64(bytes, "embedding cache payload bytes")?,
        })
    })
}

pub fn embedding_cache_space_hash(
    provider: &str,
    provider_config: &JsonValue,
    dimensions: u64,
    semantic_identity: &str,
) -> StorageResult<HashId> {
    let canonical = canonical_json(provider_config.clone());
    let config = serde_json::to_vec(&canonical).map_err(|error| {
        StorageError::corrupt(format!(
            "failed to encode canonical providerConfig: {error}"
        ))
    })?;
    let mut hasher = RecordHasher::new("EMBEDDING_SPACE", 6);
    hasher.field(provider.as_bytes());
    hasher.field(&config);
    hasher.field(&u64_bytes(dimensions));
    hasher.field(b"FLOAT32");
    hasher.field(semantic_identity.as_bytes());
    hasher.field(&u64_bytes(EMBEDDING_CACHE_ENCODING_VERSION));
    Ok(hasher.finish())
}

pub fn embedding_text_hash(text: &str) -> HashId {
    let mut hasher = RecordHasher::new("EMBEDDING_TEXT", 1);
    hasher.field(text.as_bytes());
    hasher.finish()
}

pub fn embedding_cache_lookup(
    connection: &Connection,
    space_hash: HashId,
    text: &str,
    dimensions: usize,
) -> StorageResult<Option<Vec<f32>>> {
    let policy = embedding_cache_policy(connection)?;
    if !policy.enabled {
        return Ok(None);
    }
    let text_hash = embedding_text_hash(text);
    let Some(row) = cached_embedding_row(connection, space_hash, text_hash)? else {
        return Ok(None);
    };
    decode_cached_vector(
        text,
        dimensions,
        row.text_bytes,
        row.dimension,
        row.coordinate_type,
        &row.vector_blob,
        row.payload_bytes,
    )
}

pub fn embedding_cache_publish(
    connection: &Connection,
    space_hash: HashId,
    dimensions: usize,
    entries: &[EmbeddingCacheEntry],
) -> StorageResult<()> {
    require_embedding_cache_format(connection)?;
    if entries.is_empty() {
        return Ok(());
    }
    validate_dimension(dimensions)?;
    for entry in entries {
        validate_vector(&entry.vector, dimensions)?;
        checked_i64(entry.text.len(), "embedding text length")?;
        checked_i64(
            std::mem::size_of_val(entry.vector.as_slice()),
            "embedding payload",
        )?;
    }
    with_savepoint(connection, "lithograph_embedding_cache_publish", || {
        // Read the operational policy only after the write SAVEPOINT is
        // established. This serializes publish with concurrent configure
        // calls, so a completed disable/budget reduction cannot be bypassed
        // by a stale pre-transaction policy snapshot.
        let policy = embedding_cache_policy(connection)?;
        if !policy.enabled {
            return Ok(());
        }
        for entry in entries {
            publish_embedding_entry(connection, space_hash, dimensions, entry)?;
        }
        evict_to_budget(connection, policy.max_bytes)
    })
}

fn publish_embedding_entry(
    connection: &Connection,
    space_hash: HashId,
    dimensions: usize,
    entry: &EmbeddingCacheEntry,
) -> StorageResult<()> {
    validate_vector(&entry.vector, dimensions)?;
    let text_hash = embedding_text_hash(&entry.text);
    if !prepare_embedding_slot(connection, space_hash, text_hash, dimensions, entry)? {
        return Ok(());
    }
    connection.execute(
        "INSERT INTO main._lithograph_embedding_cache(space_hash, text_hash, text_bytes, dimension, coordinate_type, vector_blob, payload_bytes) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7)",
        params![
            space_hash.as_bytes().as_slice(),
            text_hash.as_bytes().as_slice(),
            checked_i64(entry.text.len(), "embedding text length")?,
            checked_i64(dimensions, "embedding dimension")?,
            FLOAT32_COORDINATE_TYPE,
            encode_embedding_vector(&entry.vector),
            checked_i64(std::mem::size_of_val(entry.vector.as_slice()), "embedding payload")?,
        ],
    )?;
    Ok(())
}

fn prepare_embedding_slot(
    connection: &Connection,
    space_hash: HashId,
    text_hash: HashId,
    dimensions: usize,
    entry: &EmbeddingCacheEntry,
) -> StorageResult<bool> {
    let Some(existing) = cached_embedding_row(connection, space_hash, text_hash)? else {
        return Ok(true);
    };
    if existing.text_bytes != checked_i64(entry.text.len(), "embedding text length")? {
        return Err(StorageError::corrupt(
            "embedding cache text hash collision was detected",
        ));
    }
    if decode_cached_vector(
        &entry.text,
        dimensions,
        existing.text_bytes,
        existing.dimension,
        existing.coordinate_type,
        &existing.vector_blob,
        existing.payload_bytes,
    )?
    .is_some()
    {
        return Ok(false);
    }
    connection.execute(
        "DELETE FROM main._lithograph_embedding_cache WHERE space_hash = ?1 AND text_hash = ?2",
        params![
            space_hash.as_bytes().as_slice(),
            text_hash.as_bytes().as_slice()
        ],
    )?;
    Ok(true)
}

fn cached_embedding_row(
    connection: &Connection,
    space_hash: HashId,
    text_hash: HashId,
) -> StorageResult<Option<CachedEmbeddingRow>> {
    connection
        .query_row(
            "SELECT text_bytes, dimension, coordinate_type, vector_blob, payload_bytes FROM main._lithograph_embedding_cache WHERE space_hash = ?1 AND text_hash = ?2",
            params![space_hash.as_bytes().as_slice(), text_hash.as_bytes().as_slice()],
            |row| {
                Ok(CachedEmbeddingRow {
                    text_bytes: row.get(0)?,
                    dimension: row.get(1)?,
                    coordinate_type: row.get(2)?,
                    vector_blob: row.get(3)?,
                    payload_bytes: row.get(4)?,
                })
            },
        )
        .optional()
        .map_err(StorageError::from)
}

pub fn embedding_query_cache_lookup(
    connection: &Connection,
    space_hash: HashId,
    text: &str,
    dimensions: usize,
) -> StorageResult<Option<Vec<f32>>> {
    ensure_query_cache(connection)?;
    let text_hash = embedding_text_hash(text);
    let row = connection
        .query_row(
            "SELECT text_bytes, dimension, vector_blob              FROM temp._lithograph_semantic_embedding_lru              WHERE space_hash = ?1 AND text_hash = ?2",
            params![space_hash.as_bytes().as_slice(), text_hash.as_bytes().as_slice()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((text_bytes, dimension, blob)) = row else {
        return Ok(None);
    };
    let decoded = decode_cached_vector(
        text,
        dimensions,
        text_bytes,
        dimension,
        FLOAT32_COORDINATE_TYPE,
        &blob,
        checked_i64(blob.len(), "query embedding payload")?,
    )?;
    if decoded.is_some() {
        touch_query_cache(connection, space_hash, text_hash)?;
    } else {
        connection.execute(
            "DELETE FROM temp._lithograph_semantic_embedding_lru              WHERE space_hash = ?1 AND text_hash = ?2",
            params![space_hash.as_bytes().as_slice(), text_hash.as_bytes().as_slice()],
        )?;
    }
    Ok(decoded)
}

pub fn embedding_query_cache_put(
    connection: &Connection,
    space_hash: HashId,
    text: &str,
    vector: &[f32],
) -> StorageResult<()> {
    ensure_query_cache(connection)?;
    validate_vector(vector, vector.len())?;
    let text_hash = embedding_text_hash(text);
    let tick = next_query_cache_tick(connection)?;
    connection.execute(
        "INSERT INTO temp._lithograph_semantic_embedding_lru(             space_hash, text_hash, text_bytes, dimension, vector_blob, touched         ) VALUES(?1, ?2, ?3, ?4, ?5, ?6)          ON CONFLICT(space_hash, text_hash) DO UPDATE SET              text_bytes=excluded.text_bytes, dimension=excluded.dimension,              vector_blob=excluded.vector_blob, touched=excluded.touched",
        params![
            space_hash.as_bytes().as_slice(),
            text_hash.as_bytes().as_slice(),
            checked_i64(text.len(), "embedding text length")?,
            checked_i64(vector.len(), "embedding dimension")?,
            encode_embedding_vector(vector),
            tick,
        ],
    )?;
    connection.execute(
        "DELETE FROM temp._lithograph_semantic_embedding_lru WHERE (space_hash, text_hash) IN (             SELECT space_hash, text_hash FROM temp._lithograph_semantic_embedding_lru              ORDER BY touched ASC, hex(space_hash), hex(text_hash)              LIMIT MAX(0, (SELECT count(*) FROM temp._lithograph_semantic_embedding_lru) - ?1)         )",
        [QUERY_CACHE_MAX_ENTRIES],
    )?;
    Ok(())
}

fn storage_format(connection: &Connection) -> StorageResult<i64> {
    connection
        .query_row(
            "SELECT storage_format FROM main._lithograph_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(StorageError::from)
}

fn evict_to_budget(connection: &Connection, max_bytes: u64) -> StorageResult<()> {
    let max_bytes = i64::try_from(max_bytes)
        .map_err(|_| StorageError::corrupt("embedding cache max_bytes exceeds INTEGER64"))?;
    let mut used: i64 = connection.query_row(
        "SELECT COALESCE(sum(payload_bytes), 0) FROM main._lithograph_embedding_cache",
        [],
        |row| row.get(0),
    )?;
    while used > max_bytes {
        let (cutoff, released) = eviction_batch(connection, used, max_bytes)?;
        let deleted = connection.execute(
            "DELETE FROM main._lithograph_embedding_cache WHERE entry_id <= ?1",
            [cutoff],
        )?;
        if deleted == 0 {
            return Err(StorageError::corrupt(
                "embedding cache capacity accounting cannot make progress",
            ));
        }
        used = used.saturating_sub(released);
    }
    Ok(())
}

fn eviction_batch(connection: &Connection, used: i64, max_bytes: i64) -> StorageResult<(i64, i64)> {
    let mut statement = connection.prepare(
        "SELECT entry_id, payload_bytes FROM main._lithograph_embedding_cache ORDER BY entry_id LIMIT 256",
    )?;
    let rows = statement.query_map([], |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)))?;
    let mut cutoff = None;
    let mut released = 0_i64;
    for row in rows {
        let (entry_id, payload_bytes) = row?;
        if payload_bytes < 0 {
            return Err(StorageError::corrupt(
                "embedding cache payload_bytes is negative",
            ));
        }
        cutoff = Some(entry_id);
        released = released
            .checked_add(payload_bytes)
            .ok_or_else(|| StorageError::corrupt("embedding cache capacity accounting overflow"))?;
        if used.saturating_sub(released) <= max_bytes {
            break;
        }
    }
    cutoff.map(|cutoff| (cutoff, released)).ok_or_else(|| {
        StorageError::corrupt("embedding cache capacity accounting cannot make progress")
    })
}

fn ensure_query_cache(connection: &Connection) -> StorageResult<()> {
    connection.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _lithograph_semantic_embedding_lru(             space_hash BLOB NOT NULL CHECK(length(space_hash)=32),             text_hash BLOB NOT NULL CHECK(length(text_hash)=32),             text_bytes INTEGER NOT NULL CHECK(text_bytes>=0),             dimension INTEGER NOT NULL CHECK(dimension BETWEEN 1 AND 4096),             vector_blob BLOB NOT NULL,             touched INTEGER NOT NULL CHECK(touched>0),             PRIMARY KEY(space_hash, text_hash)         ) WITHOUT ROWID;         CREATE TEMP TABLE IF NOT EXISTS _lithograph_semantic_embedding_lru_clock(             id INTEGER PRIMARY KEY CHECK(id=1), tick INTEGER NOT NULL CHECK(tick>0)         );         INSERT OR IGNORE INTO temp._lithograph_semantic_embedding_lru_clock(id, tick) VALUES(1, 1);",
    )?;
    Ok(())
}

fn next_query_cache_tick(connection: &Connection) -> StorageResult<i64> {
    connection.execute(
        "UPDATE temp._lithograph_semantic_embedding_lru_clock SET tick=tick+1 WHERE id=1",
        [],
    )?;
    connection
        .query_row(
            "SELECT tick FROM temp._lithograph_semantic_embedding_lru_clock WHERE id=1",
            [],
            |row| row.get(0),
        )
        .map_err(StorageError::from)
}

fn touch_query_cache(
    connection: &Connection,
    space_hash: HashId,
    text_hash: HashId,
) -> StorageResult<()> {
    let tick = next_query_cache_tick(connection)?;
    connection.execute(
        "UPDATE temp._lithograph_semantic_embedding_lru SET touched=?3          WHERE space_hash=?1 AND text_hash=?2",
        params![
            space_hash.as_bytes().as_slice(),
            text_hash.as_bytes().as_slice(),
            tick
        ],
    )?;
    Ok(())
}

fn decode_cached_vector(
    text: &str,
    expected_dimensions: usize,
    text_bytes: i64,
    dimension: i64,
    coordinate_type: i64,
    blob: &[u8],
    payload_bytes: i64,
) -> StorageResult<Option<Vec<f32>>> {
    validate_dimension(expected_dimensions)?;
    let expected_payload = expected_dimensions
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| StorageError::corrupt("embedding payload size overflow"))?;
    if text_bytes != checked_i64(text.len(), "embedding text length")?
        || dimension != checked_i64(expected_dimensions, "embedding dimension")?
        || coordinate_type != FLOAT32_COORDINATE_TYPE
        || payload_bytes != checked_i64(expected_payload, "embedding payload")?
        || blob.len() != expected_payload
    {
        return Ok(None);
    }
    let mut vector = Vec::with_capacity(expected_dimensions);
    for chunk in blob.as_chunks::<4>().0 {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return Ok(None);
        }
        vector.push(value);
    }
    Ok(Some(vector))
}

fn validate_dimension(dimensions: usize) -> StorageResult<()> {
    if (1..=4096).contains(&dimensions) {
        Ok(())
    } else {
        Err(StorageError::corrupt(
            "embedding dimension is outside supported range",
        ))
    }
}

fn validate_vector(vector: &[f32], dimensions: usize) -> StorageResult<()> {
    validate_dimension(dimensions)?;
    if vector.len() != dimensions || vector.iter().any(|value| !value.is_finite()) {
        return Err(StorageError::corrupt(
            "embedding provider result does not satisfy the FLOAT32 vector contract",
        ));
    }
    Ok(())
}

pub(crate) fn encode_embedding_vector(vector: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(std::mem::size_of_val(vector));
    for value in vector {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
}

fn canonical_json(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(values) => {
            JsonValue::Array(values.into_iter().map(canonical_json).collect())
        }
        JsonValue::Object(values) => {
            let mut ordered = BTreeMap::new();
            for (key, value) in values {
                ordered.insert(key, canonical_json(value));
            }
            JsonValue::Object(ordered.into_iter().collect())
        }
        value => value,
    }
}

fn nonnegative_u64(value: i64, role: &str) -> StorageResult<u64> {
    u64::try_from(value)
        .map_err(|_| StorageError::corrupt(format!("{role} is negative or out of range")))
}

fn checked_i64(value: usize, role: &str) -> StorageResult<i64> {
    i64::try_from(value).map_err(|_| StorageError::corrupt(format!("{role} exceeds INTEGER64")))
}

fn with_savepoint<T>(
    connection: &Connection,
    name: &str,
    operation: impl FnOnce() -> StorageResult<T>,
) -> StorageResult<T> {
    connection.execute_batch(&format!("SAVEPOINT {name}"))?;
    match operation() {
        Ok(value) => match connection.execute_batch(&format!("RELEASE {name}")) {
            Ok(()) => Ok(value),
            Err(release_error) => {
                rollback_savepoint(connection, name)?;
                Err(release_error.into())
            }
        },
        Err(error) => {
            rollback_savepoint(connection, name)?;
            Err(error)
        }
    }
}

fn rollback_savepoint(connection: &Connection, name: &str) -> StorageResult<()> {
    if let Err(error) = connection.execute_batch(&format!("ROLLBACK TO {name}")) {
        return fail_closed_after_savepoint_error(connection, "rollback", error);
    }
    if let Err(error) = connection.execute_batch(&format!("RELEASE {name}")) {
        return fail_closed_after_savepoint_error(connection, "release", error);
    }
    Ok(())
}

fn fail_closed_after_savepoint_error(
    connection: &Connection,
    step: &str,
    error: rusqlite::Error,
) -> StorageResult<()> {
    let outer_rollback = if connection.execute_batch("ROLLBACK").is_ok() {
        "full SQLite rollback executed"
    } else {
        "full SQLite rollback also failed"
    };
    Err(StorageError::corrupt(format!(
        "embedding cache savepoint {step} failed ({error}); {outer_rollback}"
    )))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cache_connection() -> Connection {
        let connection = Connection::open_in_memory().expect("in-memory SQLite");
        connection
            .execute_batch(
                r#"CREATE TABLE main._lithograph_meta(
                     id INTEGER PRIMARY KEY CHECK(id=1),
                     magic TEXT NOT NULL,
                     database_id TEXT NOT NULL,
                     storage_format INTEGER NOT NULL,
                     "semantic.embedding_cache.enabled" INTEGER NULL
                         CHECK("semantic.embedding_cache.enabled" IS NULL
                           OR "semantic.embedding_cache.enabled" IN (0,1)),
                     "semantic.embedding_cache.max_bytes" INTEGER NULL
                         CHECK("semantic.embedding_cache.max_bytes" IS NULL
                           OR "semantic.embedding_cache.max_bytes" > 0)
                 );
                 INSERT INTO main._lithograph_meta(
                     id, magic, database_id, storage_format
                 ) VALUES(
                     1, 'lithograph-format-v1',
                     '00000000-0000-4000-8000-000000000013', 4
                 );"#,
            )
            .expect("format4 metadata");
        super::super::schema::create_format4_schema(&connection).expect("format4 cache schema");
        connection
    }

    fn space() -> HashId {
        embedding_cache_space_hash(
            "synthetic",
            &serde_json::json!({"model":"v1"}),
            2,
            "synthetic/v1",
        )
        .expect("space hash")
    }

    #[test]
    fn space_hash_is_canonical() {
        let left = serde_json::json!({"b":[2,{"z":true,"a":null}],"a":1});
        let right = serde_json::json!({"a":1,"b":[2,{"a":null,"z":true}]});
        assert_eq!(
            embedding_cache_space_hash("p", &left, 3, "id").expect("left"),
            embedding_cache_space_hash("p", &right, 3, "id").expect("right")
        );
        assert_ne!(
            embedding_cache_space_hash("p", &left, 3, "id").expect("base"),
            embedding_cache_space_hash("p", &left, 4, "id").expect("dimension")
        );
        assert_ne!(
            embedding_cache_space_hash("p", &left, 3, "id").expect("base"),
            embedding_cache_space_hash("q", &left, 3, "id").expect("provider")
        );
        assert_ne!(
            embedding_cache_space_hash("p", &left, 3, "id").expect("base"),
            embedding_cache_space_hash("p", &left, 3, "id2").expect("identity")
        );
    }

    #[test]
    fn text_hash_uses_exact_utf8_bytes() {
        assert_ne!(embedding_text_hash(" A"), embedding_text_hash("A"));
        assert_ne!(embedding_text_hash("é"), embedding_text_hash("e\u{301}"));
        assert_ne!(embedding_text_hash("A"), embedding_text_hash("a"));
    }

    #[test]
    fn policy_stats_clear_and_disabled_publish_are_consistent() {
        let connection = cache_connection();
        assert_eq!(
            embedding_cache_policy(&connection).expect("default policy"),
            EmbeddingCachePolicy {
                enabled: true,
                max_bytes: DEFAULT_MAX_BYTES,
            }
        );
        let policy =
            embedding_cache_configure(&connection, Some(false), Some(64)).expect("configure");
        assert_eq!(
            policy,
            EmbeddingCachePolicy {
                enabled: false,
                max_bytes: 64,
            }
        );
        embedding_cache_publish(
            &connection,
            space(),
            2,
            &[EmbeddingCacheEntry {
                text: "hidden".to_owned(),
                vector: vec![1.0, 2.0],
            }],
        )
        .expect("disabled publish");
        assert_eq!(
            embedding_cache_stats(&connection).expect("disabled stats"),
            EmbeddingCacheStats {
                policy,
                used_bytes: 0,
                entries: 0,
                spaces: 0,
            }
        );

        embedding_cache_configure(&connection, Some(true), None).expect("enable");
        embedding_cache_publish(
            &connection,
            space(),
            2,
            &[EmbeddingCacheEntry {
                text: "visible".to_owned(),
                vector: vec![1.0, 2.0],
            }],
        )
        .expect("publish");
        let stats = embedding_cache_stats(&connection).expect("stats");
        assert_eq!(stats.used_bytes, 8);
        assert_eq!(stats.entries, 1);
        assert_eq!(stats.spaces, 1);
        let cleared = embedding_cache_clear(&connection).expect("clear");
        assert_eq!(
            cleared,
            EmbeddingCacheClear {
                deleted_entries: 1,
                released_payload_bytes: 8,
            }
        );
        assert_eq!(
            embedding_cache_stats(&connection)
                .expect("cleared stats")
                .entries,
            0
        );
    }

    #[test]
    fn fifo_budget_evicts_oldest_persistent_entry() {
        let connection = cache_connection();
        embedding_cache_configure(&connection, Some(true), Some(8)).expect("budget");
        embedding_cache_publish(
            &connection,
            space(),
            2,
            &[
                EmbeddingCacheEntry {
                    text: "old".to_owned(),
                    vector: vec![1.0, 0.0],
                },
                EmbeddingCacheEntry {
                    text: "new".to_owned(),
                    vector: vec![0.0, 1.0],
                },
            ],
        )
        .expect("publish with eviction");
        assert!(
            embedding_cache_lookup(&connection, space(), "old", 2)
                .expect("old lookup")
                .is_none()
        );
        assert_eq!(
            embedding_cache_lookup(&connection, space(), "new", 2).expect("new lookup"),
            Some(vec![0.0, 1.0])
        );
        let stats = embedding_cache_stats(&connection).expect("stats");
        assert_eq!(stats.used_bytes, 8);
        assert_eq!(stats.entries, 1);
    }

    #[test]
    fn fifo_budget_evicts_across_multiple_oldest_batches() {
        let connection = cache_connection();
        let space = embedding_cache_space_hash(
            "synthetic",
            &serde_json::json!({"model":"v1"}),
            1,
            "synthetic/v1",
        )
        .expect("1d space hash");
        embedding_cache_configure(&connection, Some(true), Some(8)).expect("budget");
        let entries = (0..300)
            .map(|index| EmbeddingCacheEntry {
                text: format!("item-{index:03}"),
                vector: vec![index as f32],
            })
            .collect::<Vec<_>>();
        embedding_cache_publish(&connection, space, 1, &entries).expect("batched FIFO eviction");

        let stats = embedding_cache_stats(&connection).expect("stats");
        assert_eq!(stats.used_bytes, 8);
        assert_eq!(stats.entries, 2);
        assert!(
            embedding_cache_lookup(&connection, space, "item-297", 1)
                .expect("old lookup")
                .is_none()
        );
        assert_eq!(
            embedding_cache_lookup(&connection, space, "item-298", 1).expect("newer lookup"),
            Some(vec![298.0])
        );
        assert_eq!(
            embedding_cache_lookup(&connection, space, "item-299", 1).expect("newest lookup"),
            Some(vec![299.0])
        );
    }

    #[test]
    fn disabled_policy_preserves_rows_until_reenabled_capacity_maintenance() {
        let connection = cache_connection();
        embedding_cache_publish(
            &connection,
            space(),
            2,
            &[
                EmbeddingCacheEntry {
                    text: "first".to_owned(),
                    vector: vec![1.0, 0.0],
                },
                EmbeddingCacheEntry {
                    text: "second".to_owned(),
                    vector: vec![0.0, 1.0],
                },
            ],
        )
        .expect("seed persistent cache");
        let seeded = embedding_cache_stats(&connection).expect("seeded stats");
        assert_eq!(seeded.entries, 2);
        assert_eq!(seeded.used_bytes, 16);

        let disabled =
            embedding_cache_configure(&connection, Some(false), Some(4)).expect("disable");
        assert_eq!(
            disabled,
            EmbeddingCachePolicy {
                enabled: false,
                max_bytes: 4,
            }
        );
        let retained = embedding_cache_stats(&connection).expect("disabled retained stats");
        assert_eq!(retained.entries, 2);
        assert_eq!(retained.used_bytes, 16);
        assert!(
            embedding_cache_lookup(&connection, space(), "first", 2)
                .expect("disabled lookup")
                .is_none()
        );

        embedding_cache_configure(&connection, Some(true), None).expect("reenable");
        let evicted = embedding_cache_stats(&connection).expect("reenabled stats");
        assert_eq!(evicted.entries, 0);
        assert_eq!(evicted.used_bytes, 0);
    }

    #[test]
    fn corrupted_derived_row_is_a_read_miss_and_publish_repairs_it() {
        let connection = cache_connection();
        let text = "repair";
        let text_hash = embedding_text_hash(text);
        connection
            .execute(
                "INSERT INTO main._lithograph_embedding_cache(space_hash,text_hash,text_bytes,dimension,coordinate_type,vector_blob,payload_bytes) VALUES(?1,?2,?3,2,5,?4,8)",
                params![
                    space().as_bytes().as_slice(),
                    text_hash.as_bytes().as_slice(),
                    i64::try_from(text.len()).expect("text length"),
                    vec![0_u8; 7],
                ],
            )
            .expect("corrupt derived row");
        assert!(
            embedding_cache_lookup(&connection, space(), text, 2)
                .expect("corrupt lookup")
                .is_none()
        );
        embedding_cache_publish(
            &connection,
            space(),
            2,
            &[EmbeddingCacheEntry {
                text: text.to_owned(),
                vector: vec![3.0, 4.0],
            }],
        )
        .expect("repair publish");
        assert_eq!(
            embedding_cache_lookup(&connection, space(), text, 2).expect("repaired lookup"),
            Some(vec![3.0, 4.0])
        );
    }

    #[test]
    fn invalid_publish_batch_is_rejected_before_any_row_is_written() {
        let connection = cache_connection();
        let result = embedding_cache_publish(
            &connection,
            space(),
            2,
            &[
                EmbeddingCacheEntry {
                    text: "valid".to_owned(),
                    vector: vec![1.0, 2.0],
                },
                EmbeddingCacheEntry {
                    text: "invalid".to_owned(),
                    vector: vec![f32::NAN, 3.0],
                },
            ],
        );
        assert!(result.is_err());
        assert_eq!(
            embedding_cache_stats(&connection)
                .expect("stats after rejected publish")
                .entries,
            0
        );
    }

    #[test]
    fn query_lru_remains_connection_local_without_persistent_publish() {
        let first = cache_connection();
        let second = cache_connection();
        embedding_query_cache_put(&first, space(), "query-only", &[5.0, 6.0])
            .expect("put query lru");
        assert_eq!(
            embedding_query_cache_lookup(&first, space(), "query-only", 2)
                .expect("same connection lookup"),
            Some(vec![5.0, 6.0])
        );
        assert!(
            embedding_query_cache_lookup(&second, space(), "query-only", 2)
                .expect("other connection lookup")
                .is_none()
        );
        let persistent: i64 = first
            .query_row(
                "SELECT count(*) FROM main._lithograph_embedding_cache",
                [],
                |row| row.get(0),
            )
            .expect("persistent count");
        assert_eq!(persistent, 0);
    }
}
