use super::*;
use crate::query::resources::{MemoryReservation, QueryResources};

/// A query reserves this workspace before collecting entries or loading the
/// derived cache. Entry levels are at most 32; the bound includes spare Vec
/// capacity, B-tree nodes, search queues, entry clones and JSON serialization.
/// SQLite's own TEMP pages are not part of this Rust workspace.
pub(in crate::query::semantic_index) fn hnsw_workspace_bytes(
    entry_count: usize,
    dimension: usize,
    maximum_connections: usize,
    persisted_json_bytes: u64,
) -> QueryResult<u64> {
    let vector = checked_mul(dimension as u64, 4)?;
    let layer_headers = 33 * std::mem::size_of::<Vec<i64>>() as u64;
    let neighbor_capacity = checked_mul(maximum_connections as u64, 34 * 16)?;
    let entry = checked_add(
        std::mem::size_of::<VectorCacheEntry>() as u64,
        checked_add(vector, checked_add(layer_headers, neighbor_capacity)?)?,
    )?;
    let map_node = checked_add(
        checked_mul(16, std::mem::size_of::<(i64, VectorCacheEntry)>() as u64)?,
        256,
    )?;
    let queues_and_hits = 1024;
    let per_entry = checked_add(
        checked_mul(entry, 4)?,
        checked_add(
            checked_add(map_node, queues_and_hits)?,
            checked_mul(dimension as u64, 32)?,
        )?,
    )?;
    // JSON may contain many empty inner arrays or grow a Vec geometrically.
    // Account decoded containers and simultaneous entry/JSON copies by bytes,
    // including malformed derived rows that do not match the query dimension.
    checked_add(
        checked_mul(entry_count as u64, per_entry)?,
        checked_mul(persisted_json_bytes, 64)?,
    )
}

#[derive(Debug)]
pub(super) struct OwnedCacheEntry {
    entry: VectorCacheEntry,
    memory: Option<MemoryReservation>,
}

impl std::ops::Deref for OwnedCacheEntry {
    type Target = VectorCacheEntry;

    fn deref(&self) -> &Self::Target {
        &self.entry
    }
}

impl<'connection, 'key> SqlVectorCache<'connection, 'key> {
    pub(super) fn new(
        connection: &'connection Connection,
        cache_key: &'key str,
        resources: Option<&QueryResources>,
    ) -> Self {
        Self {
            connection,
            cache_key,
            loaded: BTreeMap::new(),
            resources: resources.cloned(),
            loaded_memory: None,
        }
    }

    pub(super) fn entry(
        &mut self,
        owner_id: i64,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Option<OwnedCacheEntry>> {
        check_interrupted(is_interrupted)?;
        if let Some(entry) = self.loaded.get(&owner_id) {
            return clone_cache_entry(entry, self.resources.as_ref(), is_interrupted).map(Some);
        }
        #[cfg(feature = "test-support")]
        let load_started = std::time::Instant::now();
        let decoded = self.connection.query_row(
            "SELECT vector_json, level, neighbors_json FROM temp._lithograph_vector_cache WHERE cache_key = ?1 AND owner_id = ?2",
            rusqlite::params![self.cache_key, owner_id],
            |row| Ok(decode_cache_row(row, owner_id, self.resources.as_ref(), is_interrupted)),
        ).optional()?.transpose()?.flatten();
        let Some(mut decoded) = decoded else {
            return Ok(None);
        };
        let returned = clone_cache_entry(&decoded.entry, self.resources.as_ref(), is_interrupted)?;
        if let Some(resources) = &self.resources {
            let map_memory = resources.reserve(cache_map_entry_bytes()?)?;
            let mut memory = decoded.memory.take().ok_or_else(|| {
                QueryError::internal("budgeted cache entry is missing its reservation")
            })?;
            memory.merge(map_memory)?;
            if let Some(loaded_memory) = &mut self.loaded_memory {
                loaded_memory.merge(memory)?;
            } else {
                self.loaded_memory = Some(memory);
            }
        }
        self.loaded.insert(owner_id, decoded.entry);
        #[cfg(feature = "test-support")]
        crate::performance::record_vector_cache_entry_load(load_started.elapsed().as_micros());
        Ok(Some(returned))
    }
}

fn cache_map_entry_bytes() -> QueryResult<u64> {
    checked_add(
        checked_mul(16, std::mem::size_of::<(i64, VectorCacheEntry)>() as u64)?,
        256,
    )
}

fn decode_cache_row(
    row: &rusqlite::Row<'_>,
    owner_id: i64,
    resources: Option<&QueryResources>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<OwnedCacheEntry>> {
    let level = row.get::<_, i64>(1)?;
    let Ok(level) = usize::try_from(level) else {
        return Ok(None);
    };
    if level > 32 {
        return Ok(None);
    }
    let vector_ref = row.get_ref(0)?;
    let neighbors_ref = row.get_ref(2)?;
    let vector_json = vector_ref.as_str().map_err(rusqlite::Error::from)?;
    let neighbors_json = neighbors_ref.as_str().map_err(rusqlite::Error::from)?;
    let mut memory = if let Some(resources) = resources {
        let Some(bytes) =
            cache_json_decoding_bytes(vector_json, neighbors_json, level, is_interrupted)?
        else {
            return Ok(None);
        };
        Some(resources.reserve(bytes)?)
    } else {
        None
    };
    let Some(entry) =
        decode_cache_arrays(owner_id, vector_json, neighbors_json, level, is_interrupted)?
    else {
        return Ok(None);
    };
    if let Some(memory) = &mut memory {
        memory.resize(cache_entry_bytes(&entry, false)?)?;
    }
    Ok(Some(OwnedCacheEntry { entry, memory }))
}

fn decode_cache_arrays(
    owner_id: i64,
    vector_json: &str,
    neighbors_json: &str,
    level: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<VectorCacheEntry>> {
    check_interrupted(is_interrupted)?;
    let Ok(vector) = serde_json::from_str::<Vec<f32>>(vector_json) else {
        return Ok(None);
    };
    check_interrupted(is_interrupted)?;
    let Ok(neighbors) = serde_json::from_str::<Vec<Vec<i64>>>(neighbors_json) else {
        return Ok(None);
    };
    check_interrupted(is_interrupted)?;
    if vector.is_empty() || neighbors.len() != level + 1 {
        return Ok(None);
    }
    Ok(Some(VectorCacheEntry {
        owner_id,
        vector,
        level,
        neighbors,
    }))
}

fn numeric_json_shape(
    json: &str,
    floating: bool,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<(usize, usize)>> {
    let mut brackets = 0usize;
    let mut commas = 0usize;
    let punctuation: &[u8] = if floating { b"[],-+.eE" } else { b"[],-" };
    for chunk in json.as_bytes().chunks(64 * 1024) {
        check_interrupted(is_interrupted)?;
        for byte in chunk {
            if !byte.is_ascii_digit() && !byte.is_ascii_whitespace() && !punctuation.contains(byte)
            {
                return Ok(None);
            }
            brackets += usize::from(*byte == b'[');
            commas += usize::from(*byte == b',');
        }
    }
    Ok(Some((brackets, commas)))
}

pub(super) fn cache_json_decoding_bytes(
    vector_json: &str,
    neighbors_json: &str,
    level: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<u64>> {
    let Some((vector_brackets, vector_commas)) =
        numeric_json_shape(vector_json, true, is_interrupted)?
    else {
        return Ok(None);
    };
    let Some((neighbor_brackets, neighbor_commas)) =
        numeric_json_shape(neighbors_json, false, is_interrupted)?
    else {
        return Ok(None);
    };
    if vector_brackets != 1 || neighbor_brackets != level + 2 {
        return Ok(None);
    }
    decoded_array_bytes(
        vector_commas as u64,
        (neighbor_brackets - 1) as u64,
        neighbor_commas as u64,
    )
    .map(Some)
}

fn decoded_array_bytes(vector_commas: u64, layers: u64, neighbor_commas: u64) -> QueryResult<u64> {
    // serde's sequence visitor starts with a small Vec and geometrically grows
    // it. Count numeric array items in borrowed SQLite text before that growth.
    let vector_capacity = checked_mul(checked_add(vector_commas, 1)?, 2)?.max(4);
    let layer_capacity = checked_mul(layers, 2)?.max(4);
    let neighbor_capacity = checked_add(checked_mul(layers, 6)?, checked_mul(neighbor_commas, 2)?)?;
    let bytes = checked_add(
        checked_mul(vector_capacity, 4)?,
        checked_mul(layer_capacity, std::mem::size_of::<Vec<i64>>() as u64)?,
    )?;
    let bytes = checked_add(bytes, checked_mul(neighbor_capacity, 8)?)?;
    checked_add(bytes, 256 + std::mem::size_of::<VectorCacheEntry>() as u64)
}

pub(super) fn cache_entry_bytes(entry: &VectorCacheEntry, cloning: bool) -> QueryResult<u64> {
    let vector = if cloning {
        entry.vector.len()
    } else {
        entry.vector.capacity()
    };
    let layers = if cloning {
        entry.neighbors.len()
    } else {
        entry.neighbors.capacity()
    };
    let mut bytes = checked_add(
        std::mem::size_of::<VectorCacheEntry>() as u64,
        checked_mul(vector as u64, 4)?,
    )?;
    bytes = checked_add(
        bytes,
        checked_mul(layers as u64, std::mem::size_of::<Vec<i64>>() as u64)?,
    )?;
    for layer in &entry.neighbors {
        let capacity = if cloning {
            layer.len()
        } else {
            layer.capacity()
        };
        bytes = checked_add(bytes, checked_mul(capacity as u64, 8)?)?;
    }
    Ok(bytes)
}

fn clone_cache_entry(
    entry: &VectorCacheEntry,
    resources: Option<&QueryResources>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<OwnedCacheEntry> {
    check_interrupted(is_interrupted)?;
    let memory = resources
        .map(|resources| resources.reserve(cache_entry_bytes(entry, true)?))
        .transpose()?;
    let mut vector = Vec::with_capacity(entry.vector.len());
    for values in entry.vector.chunks(256) {
        check_interrupted(is_interrupted)?;
        vector.extend_from_slice(values);
    }
    let mut neighbors = Vec::with_capacity(entry.neighbors.len());
    for layer in &entry.neighbors {
        let mut copied = Vec::with_capacity(layer.len());
        for owners in layer.chunks(256) {
            check_interrupted(is_interrupted)?;
            copied.extend_from_slice(owners);
        }
        neighbors.push(copied);
    }
    check_interrupted(is_interrupted)?;
    Ok(OwnedCacheEntry {
        entry: VectorCacheEntry {
            owner_id: entry.owner_id,
            vector,
            level: entry.level,
            neighbors,
        },
        memory,
    })
}

pub(super) fn vector_cache_meta(
    connection: &Connection,
    cache_key: &str,
) -> QueryResult<Option<VectorCacheMeta>> {
    connection
        .query_row(
            "SELECT entry_owner_id, max_level, entry_count FROM temp._lithograph_vector_cache_meta WHERE cache_key = ?1 AND complete = 1",
            [cache_key],
            |row| {
                let max_level = row.get::<_, i64>(1)?;
                let entry_count = row.get::<_, i64>(2)?;
                Ok((row.get::<_, Option<i64>>(0)?, max_level, entry_count))
            },
        )
        .optional()?
        .map(|(entry_owner_id, max_level, entry_count)| {
            Ok(VectorCacheMeta {
                entry_owner_id,
                max_level: usize::try_from(max_level).map_err(|_| {
                    QueryError::internal("derived HNSW cache has an invalid maximum level")
                })?,
                entry_count: usize::try_from(entry_count).map_err(|_| {
                    QueryError::internal("derived HNSW cache has an invalid entry count")
                })?,
            })
        })
        .transpose()
}

pub(super) fn invalidate_vector_cache(connection: &Connection, cache_key: &str) -> QueryResult<()> {
    connection.execute(
        "DELETE FROM temp._lithograph_vector_cache_meta WHERE cache_key = ?1",
        [cache_key],
    )?;
    connection.execute(
        "DELETE FROM temp._lithograph_vector_cache WHERE cache_key = ?1",
        [cache_key],
    )?;
    Ok(())
}

pub(super) fn empty_vector_cache_valid(
    connection: &Connection,
    cache_key: &str,
    meta: VectorCacheMeta,
) -> QueryResult<bool> {
    if meta.entry_owner_id.is_some() || meta.max_level != 0 {
        return Ok(false);
    }
    let has_rows = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM temp._lithograph_vector_cache WHERE cache_key=?1 LIMIT 1)",
        [cache_key],
        |row| row.get::<_, bool>(0),
    )?;
    Ok(!has_rows)
}

pub(super) fn persist_vector_cache(
    connection: &Connection,
    cache_key: &str,
    entries: &[VectorCacheEntry],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    for entry in entries {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        let vector_json = serde_json::to_string(&entry.vector).map_err(|error| {
            QueryError::internal(format!("failed to encode HNSW vector: {error}"))
        })?;
        check_interrupted(is_interrupted)?;
        let neighbors_json = serde_json::to_string(&entry.neighbors).map_err(|error| {
            QueryError::internal(format!("failed to encode HNSW neighbors: {error}"))
        })?;
        check_interrupted(is_interrupted)?;
        let level = i64::try_from(entry.level)
            .map_err(|_| QueryError::internal("HNSW level is too large to persist"))?;
        connection.execute(
            "INSERT INTO temp._lithograph_vector_cache(cache_key, owner_id, vector_json, level, neighbors_json) VALUES(?1, ?2, ?3, ?4, ?5)",
            rusqlite::params![cache_key, entry.owner_id, vector_json, level, neighbors_json],
        )?;
    }
    let max_level = entries.iter().map(|entry| entry.level).max().unwrap_or(0);
    let entry_owner_id = entries
        .iter()
        .filter(|entry| entry.level == max_level)
        .map(|entry| entry.owner_id)
        .min();
    let max_level = i64::try_from(max_level)
        .map_err(|_| QueryError::internal("HNSW maximum level is too large to persist"))?;
    let entry_count = i64::try_from(entries.len())
        .map_err(|_| QueryError::internal("HNSW entry count is too large to persist"))?;
    if is_interrupted() {
        return Err(QueryError::interrupted());
    }
    connection.execute(
        "INSERT OR REPLACE INTO temp._lithograph_vector_cache_meta(cache_key, entry_owner_id, max_level, entry_count, complete) VALUES(?1, ?2, ?3, ?4, 1)",
        rusqlite::params![cache_key, entry_owner_id, max_level, entry_count],
    )?;
    Ok(())
}

pub(super) fn vector_cache_links_valid(
    entries: &BTreeMap<i64, VectorCacheEntry>,
    max_level: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<bool> {
    for item in entries.values() {
        check_interrupted(is_interrupted)?;
        if item.level > max_level {
            return Ok(false);
        }
        for (level, neighbors) in item.neighbors.iter().enumerate() {
            for neighbor_id in neighbors {
                check_interrupted(is_interrupted)?;
                if *neighbor_id == item.owner_id
                    || entries
                        .get(neighbor_id)
                        .is_none_or(|neighbor| neighbor.level < level)
                {
                    return Ok(false);
                }
            }
        }
    }
    Ok(true)
}
