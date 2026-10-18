use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use rusqlite::Connection;

use super::similarity::vector_similarity_numbers_controlled;
use super::*;
use crate::query::check_interrupted;
use crate::query::resources::{ExecutionControl, checked_add, checked_mul};

mod cache;
mod search_memory;
pub(super) use cache::hnsw_workspace_bytes;
use cache::{
    empty_vector_cache_valid, invalidate_vector_cache, persist_vector_cache,
    vector_cache_links_valid, vector_cache_meta,
};
use search_memory::{
    ordered_hnsw_items, ordered_hnsw_items_reserved, reserve_hit_slot, reserve_hits, sort_hnsw_hits,
};

#[derive(Debug, Clone)]
struct VectorCacheEntry {
    owner_id: i64,
    vector: Vec<f32>,
    level: usize,
    neighbors: Vec<Vec<i64>>,
}

#[derive(Debug, Clone, Copy)]
struct VectorCacheMeta {
    entry_owner_id: Option<i64>,
    max_level: usize,
    entry_count: usize,
}

struct SqlVectorCache<'connection, 'key> {
    connection: &'connection Connection,
    cache_key: &'key str,
    loaded: BTreeMap<i64, VectorCacheEntry>,
    resources: Option<crate::query::resources::QueryResources>,
    loaded_memory: Option<crate::query::resources::MemoryReservation>,
}

#[derive(Debug, Clone, Copy)]
struct VectorQueueItem {
    owner_id: i64,
    score: f64,
}

impl PartialEq for VectorQueueItem {
    fn eq(&self, other: &Self) -> bool {
        self.owner_id == other.owner_id && self.score.to_bits() == other.score.to_bits()
    }
}

impl Eq for VectorQueueItem {}

impl PartialOrd for VectorQueueItem {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

impl Ord for VectorQueueItem {
    fn cmp(&self, other: &Self) -> Ordering {
        self.score
            .total_cmp(&other.score)
            .then_with(|| other.owner_id.cmp(&self.owner_id))
    }
}

struct HnswLayerResult {
    items: Vec<VectorQueueItem>,
    visited_count: usize,
    _memory: Option<MemoryReservation>,
}

#[derive(Debug, Clone)]
pub(super) struct ManagedHnswEntry {
    pub(super) owner_id: i64,
    pub(super) vector: Vec<f32>,
}

struct HnswSearchState {
    visited: BTreeSet<i64>,
    candidates: BinaryHeap<VectorQueueItem>,
    nearest: BinaryHeap<Reverse<VectorQueueItem>>,
    memory: Option<MemoryReservation>,
}

fn vector_cache_key(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    dimension: usize,
) -> QueryResult<String> {
    let base = semantic_cache_digest(snapshot, index, b"LITHOGRAPH_HNSW_CACHE_V2")?;
    Ok(format!("{base}:{dimension}"))
}

fn ensure_vector_cache_tables(connection: &Connection) -> QueryResult<()> {
    connection.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _lithograph_vector_cache_meta(\
             cache_key TEXT PRIMARY KEY, entry_owner_id INTEGER, max_level INTEGER NOT NULL, entry_count INTEGER NOT NULL, complete INTEGER NOT NULL\
         ) WITHOUT ROWID;\
         CREATE TEMP TABLE IF NOT EXISTS _lithograph_vector_cache(\
             cache_key TEXT NOT NULL, owner_id INTEGER NOT NULL, vector_json TEXT NOT NULL, level INTEGER NOT NULL, neighbors_json TEXT NOT NULL,\
             PRIMARY KEY(cache_key, owner_id)\
         ) WITHOUT ROWID;",
    )?;
    Ok(())
}

pub(super) fn build_managed_vector_cache_controlled(
    connection: &Connection,
    cache_key: &str,
    similarity: &str,
    entries: Vec<ManagedHnswEntry>,
    is_interrupted: &dyn Fn() -> bool,
    resources: Option<&crate::query::resources::QueryResources>,
) -> QueryResult<bool> {
    check_interrupted(is_interrupted)?;
    ensure_vector_cache_tables(connection)?;
    if let Some(meta) = vector_cache_meta(connection, cache_key)?
        && managed_vector_cache_matches(
            connection,
            cache_key,
            meta,
            &entries,
            is_interrupted,
            resources,
        )?
    {
        return Ok(false);
    }
    invalidate_vector_cache(connection, cache_key)?;
    let mut converted = Vec::with_capacity(entries.len());
    for entry in entries {
        check_interrupted(is_interrupted)?;
        converted.push(VectorCacheEntry {
            owner_id: entry.owner_id,
            vector: entry.vector,
            level: 0,
            neighbors: vec![Vec::new()],
        });
    }
    let mut entries = converted;
    build_hnsw_graph_with_parameters(similarity, 16, 100, &mut entries, is_interrupted)?;
    let result = persist_vector_cache(connection, cache_key, &entries, is_interrupted);
    if result.is_err() {
        let _ = invalidate_vector_cache(connection, cache_key);
    }
    result.map(|()| true)
}

fn managed_vector_cache_matches(
    connection: &Connection,
    cache_key: &str,
    meta: VectorCacheMeta,
    expected: &[ManagedHnswEntry],
    is_interrupted: &dyn Fn() -> bool,
    resources: Option<&crate::query::resources::QueryResources>,
) -> QueryResult<bool> {
    if meta.entry_count != expected.len() {
        return Ok(false);
    }
    let stored_count = connection.query_row(
        "SELECT count(*) FROM temp._lithograph_vector_cache WHERE cache_key=?1",
        [cache_key],
        |row| row.get::<_, i64>(0),
    )?;
    if usize::try_from(stored_count).ok() != Some(expected.len()) {
        return Ok(false);
    }
    if expected.is_empty() {
        return Ok(meta.entry_owner_id.is_none() && meta.max_level == 0);
    }
    let mut cache = SqlVectorCache::new(connection, cache_key, resources);
    for expected_entry in expected {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        let Some(entry) = cache.entry(expected_entry.owner_id, is_interrupted)? else {
            return Ok(false);
        };
        if entry.vector.len() != expected_entry.vector.len() {
            return Ok(false);
        }
        for (left, right) in entry
            .vector
            .chunks(256)
            .zip(expected_entry.vector.chunks(256))
        {
            check_interrupted(is_interrupted)?;
            if left != right {
                return Ok(false);
            }
        }
    }
    vector_cache_graph_valid_controlled(&cache.loaded, meta, is_interrupted)
}

pub(super) fn invalidate_managed_vector_cache(
    connection: &Connection,
    cache_key: &str,
) -> QueryResult<()> {
    ensure_vector_cache_tables(connection)?;
    invalidate_vector_cache(connection, cache_key)
}

pub(super) fn query_managed_vector_cache_controlled(
    connection: &Connection,
    cache: (&str, Option<&crate::query::resources::QueryResources>),
    query: &[f32],
    similarity: &str,
    candidate_limit: usize,
    is_interrupted: &dyn Fn() -> bool,
    mut resolve_entity: impl FnMut(i64) -> QueryResult<Option<SemanticEntity>>,
) -> QueryResult<Option<VectorCandidateResult>> {
    let (cache_key, resources) = cache;
    check_interrupted(is_interrupted)?;
    ensure_vector_cache_tables(connection)?;
    let Some(meta) = vector_cache_meta(connection, cache_key)? else {
        return Ok(None);
    };
    if meta.entry_count == 0 {
        if !empty_vector_cache_valid(connection, cache_key, meta)? {
            invalidate_vector_cache(connection, cache_key)?;
            return Ok(None);
        }
        return Ok(Some(VectorCandidateResult {
            hits: Vec::new(),
            exhaustive: true,
            memory: None,
        }));
    }
    let Some(mut cache) = open_sql_vector_cache(
        connection,
        cache_key,
        meta,
        query.len(),
        is_interrupted,
        resources,
    )?
    else {
        return Ok(None);
    };
    let entry_owner_id = meta
        .entry_owner_id
        .ok_or_else(|| QueryError::internal("validated HNSW cache is missing its entry point"))?;
    let desired = candidate_limit.max(1).min(meta.entry_count);
    search_managed_vector_cache(
        connection,
        cache_key,
        query,
        similarity,
        desired,
        meta,
        &mut cache,
        entry_owner_id,
        is_interrupted,
        &mut resolve_entity,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "managed HNSW search keeps cache state, query semantics and candidate validation explicit"
)]
fn search_managed_vector_cache(
    connection: &Connection,
    cache_key: &str,
    query: &[f32],
    similarity: &str,
    desired: usize,
    meta: VectorCacheMeta,
    cache: &mut SqlVectorCache<'_, '_>,
    entry_owner_id: i64,
    is_interrupted: &dyn Fn() -> bool,
    resolve_entity: &mut impl FnMut(i64) -> QueryResult<Option<SemanticEntity>>,
) -> QueryResult<Option<VectorCandidateResult>> {
    let mut ef_search = desired;
    loop {
        let Some(layer) = hnsw_search_sql(
            cache,
            entry_owner_id,
            meta.max_level,
            query,
            similarity,
            ef_search,
            is_interrupted,
        )?
        else {
            invalidate_vector_cache(connection, cache_key)?;
            return Ok(None);
        };
        let exhaustive = ef_search >= meta.entry_count && layer.visited_count == meta.entry_count;
        let (mut hits, memory) = managed_hits_from_layer(
            layer.items,
            is_interrupted,
            resolve_entity,
            cache.resources.as_ref(),
        )?;
        sort_hnsw_hits(&mut hits, cache.resources.as_ref(), is_interrupted)?;
        if hits.len() >= desired || exhaustive {
            return Ok(Some(VectorCandidateResult {
                hits,
                exhaustive,
                memory,
            }));
        }
        if ef_search == meta.entry_count {
            invalidate_vector_cache(connection, cache_key)?;
            return Ok(None);
        }
        let next = ef_search.saturating_mul(2).max(ef_search.saturating_add(1));
        ef_search = next.min(meta.entry_count);
    }
}

fn managed_hits_from_layer(
    items: Vec<VectorQueueItem>,
    is_interrupted: &dyn Fn() -> bool,
    resolve_entity: &mut impl FnMut(i64) -> QueryResult<Option<SemanticEntity>>,
    resources: Option<&crate::query::resources::QueryResources>,
) -> QueryResult<(Vec<SemanticHit>, Option<MemoryReservation>)> {
    let mut hits = Vec::new();
    let mut memory = reserve_hits(resources)?;
    for item in items {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if let Some(entity) = resolve_entity(item.owner_id)? {
            reserve_hit_slot(&mut hits, &mut memory)?;
            hits.push(SemanticHit {
                entity,
                score: item.score,
            });
        }
    }
    Ok((hits, memory))
}

pub(super) fn query_vector_cache_controlled(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    query: &Value,
    candidate_limit: usize,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<VectorCandidateResult>> {
    check_interrupted(is_interrupted)?;
    let connection = snapshot.connection_for_query();
    ensure_vector_cache_tables(connection)?;
    let (query_vector, _query_memory) = try_vector_numbers_controlled(query, control)?;
    let query_vector = query_vector.ok_or_else(|| {
        QueryError::semantic("SEARCH FOR requires VECTOR or LIST<INTEGER | FLOAT>")
    })?;
    let cache_key = vector_cache_key(snapshot, index, query_vector.len())?;
    let Some(meta) = vector_cache_meta(connection, &cache_key)? else {
        return Ok(None);
    };
    if meta.entry_count == 0 {
        if !empty_vector_cache_valid(connection, &cache_key, meta)? {
            invalidate_vector_cache(connection, &cache_key)?;
            return Ok(None);
        }
        return Ok(Some(VectorCandidateResult {
            hits: Vec::new(),
            exhaustive: true,
            memory: None,
        }));
    }
    let Some(mut cache) = open_sql_vector_cache(
        connection,
        &cache_key,
        meta,
        query_vector.len(),
        is_interrupted,
        control.map(ExecutionControl::resources),
    )?
    else {
        return Ok(None);
    };
    let similarity = vector_configuration(index)?.1;
    let input = VectorSearchInput {
        query: &query_vector,
        similarity,
    };
    #[cfg(feature = "test-support")]
    let search_started = std::time::Instant::now();
    let result = search_vector_cache(
        snapshot,
        graph_view,
        index,
        &input,
        &mut cache,
        meta,
        candidate_limit,
        is_interrupted,
        control,
    );
    #[cfg(feature = "test-support")]
    crate::performance::record_vector_cache_search(search_started.elapsed().as_micros());
    result
}

fn open_sql_vector_cache<'connection, 'key>(
    connection: &'connection Connection,
    cache_key: &'key str,
    meta: VectorCacheMeta,
    query_dimension: usize,
    is_interrupted: &dyn Fn() -> bool,
    resources: Option<&crate::query::resources::QueryResources>,
) -> QueryResult<Option<SqlVectorCache<'connection, 'key>>> {
    check_interrupted(is_interrupted)?;
    if meta.max_level > 32 {
        invalidate_vector_cache(connection, cache_key)?;
        return Ok(None);
    }
    let Some(entry_owner_id) = meta.entry_owner_id else {
        invalidate_vector_cache(connection, cache_key)?;
        return Ok(None);
    };
    let mut cache = SqlVectorCache::new(connection, cache_key, resources);
    let Some(entry) = cache.entry(entry_owner_id, is_interrupted)? else {
        invalidate_vector_cache(connection, cache_key)?;
        return Ok(None);
    };
    if entry.level != meta.max_level || entry.vector.len() != query_dimension {
        invalidate_vector_cache(connection, cache_key)?;
        return Ok(None);
    }
    Ok(Some(cache))
}

#[allow(
    clippy::too_many_arguments,
    reason = "cached HNSW search keeps immutable snapshot, index, graph and derived-cache state explicit"
)]
fn search_vector_cache(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &VectorSearchInput<'_>,
    cache: &mut SqlVectorCache<'_, '_>,
    meta: VectorCacheMeta,
    candidate_limit: usize,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<VectorCandidateResult>> {
    let entry_owner_id = meta
        .entry_owner_id
        .ok_or_else(|| QueryError::internal("validated HNSW cache is missing its entry point"))?;
    let desired = candidate_limit.max(1).min(meta.entry_count);
    let mut ef_search = desired;
    loop {
        let Some(layer) = hnsw_search_sql(
            cache,
            entry_owner_id,
            meta.max_level,
            input.query,
            input.similarity,
            ef_search,
            is_interrupted,
        )?
        else {
            invalidate_vector_cache(cache.connection, cache.cache_key)?;
            return Ok(None);
        };
        let exhaustive = ef_search >= meta.entry_count && layer.visited_count == meta.entry_count;
        let (mut hits, memory) = semantic_hits_from_cache(
            snapshot,
            graph_view,
            index,
            input,
            &layer.items,
            is_interrupted,
            control,
        )?;
        sort_hnsw_hits(&mut hits, cache.resources.as_ref(), is_interrupted)?;
        if hits.len() >= desired || exhaustive {
            return Ok(Some(VectorCandidateResult {
                hits,
                exhaustive,
                memory,
            }));
        }
        if ef_search == meta.entry_count {
            invalidate_vector_cache(cache.connection, cache.cache_key)?;
            return Ok(None);
        }
        let next = ef_search.saturating_mul(2).max(ef_search.saturating_add(1));
        ef_search = next.min(meta.entry_count);
    }
}

#[cfg(test)]
fn vector_cache_graph_valid(
    entries: &BTreeMap<i64, VectorCacheEntry>,
    meta: VectorCacheMeta,
) -> bool {
    vector_cache_graph_valid_controlled(entries, meta, &|| false).unwrap_or(false)
}

fn vector_cache_graph_valid_controlled(
    entries: &BTreeMap<i64, VectorCacheEntry>,
    meta: VectorCacheMeta,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<bool> {
    check_interrupted(is_interrupted)?;
    if entries.is_empty() {
        return Ok(meta.entry_count == 0 && meta.entry_owner_id.is_none() && meta.max_level == 0);
    }
    let Some(entry_owner_id) = meta.entry_owner_id else {
        return Ok(false);
    };
    if entries.len() != meta.entry_count {
        return Ok(false);
    }
    let Some(entry) = entries.get(&entry_owner_id) else {
        return Ok(false);
    };
    if entry.level != meta.max_level || meta.max_level > 32 {
        return Ok(false);
    }
    if !vector_cache_links_valid(entries, meta.max_level, is_interrupted)? {
        return Ok(false);
    }
    let mut reachable = BTreeSet::from([entry_owner_id]);
    let mut pending = vec![entry_owner_id];
    while let Some(owner_id) = pending.pop() {
        check_interrupted(is_interrupted)?;
        let Some(neighbors) = entries
            .get(&owner_id)
            .and_then(|entry| entry.neighbors.first())
        else {
            return Ok(false);
        };
        for neighbor_id in neighbors {
            check_interrupted(is_interrupted)?;
            if reachable.insert(*neighbor_id) {
                pending.push(*neighbor_id);
            }
        }
    }
    Ok(reachable.len() == entries.len())
}

fn hnsw_search_sql(
    cache: &mut SqlVectorCache<'_, '_>,
    entry_owner_id: i64,
    max_level: usize,
    query: &[f32],
    similarity: &str,
    ef_search: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<HnswLayerResult>> {
    let mut current = entry_owner_id;
    for level in (1..=max_level).rev() {
        let Some(result) = hnsw_search_layer_sql(
            cache,
            &[current],
            query,
            similarity,
            level,
            1,
            is_interrupted,
        )?
        else {
            return Ok(None);
        };
        if let Some(best) = result.items.first() {
            current = best.owner_id;
        }
    }
    hnsw_search_layer_sql(
        cache,
        &[current],
        query,
        similarity,
        0,
        ef_search.max(1),
        is_interrupted,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "lazy HNSW traversal keeps cache, graph layer, query, similarity, search width and cancellation explicit"
)]
fn hnsw_search_layer_sql(
    cache: &mut SqlVectorCache<'_, '_>,
    entry_points: &[i64],
    query: &[f32],
    similarity: &str,
    level: usize,
    ef: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<HnswLayerResult>> {
    let ef = ef.max(1);
    let Some(mut state) = initialize_sql_search(
        cache,
        entry_points,
        query,
        similarity,
        level,
        is_interrupted,
    )?
    else {
        return Ok(None);
    };
    while let Some(candidate) = state.candidates.pop() {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if state.nearest.len() >= ef
            && state
                .nearest
                .peek()
                .is_some_and(|Reverse(worst)| candidate < *worst)
        {
            break;
        }
        if !expand_sql_candidate(
            cache,
            candidate,
            query,
            similarity,
            level,
            ef,
            &mut state,
            is_interrupted,
        )? {
            return Ok(None);
        }
    }
    let (items, memory) =
        ordered_hnsw_items_reserved(state.nearest, is_interrupted, cache.resources.as_ref())?;
    Ok(Some(HnswLayerResult {
        items,
        visited_count: state.visited.len(),
        _memory: memory,
    }))
}

fn initialize_sql_search(
    cache: &mut SqlVectorCache<'_, '_>,
    entry_points: &[i64],
    query: &[f32],
    similarity: &str,
    level: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<HnswSearchState>> {
    let mut state = HnswSearchState::new(cache.resources.as_ref())?;
    for owner_id in entry_points {
        let Some(entry) = cache.entry(*owner_id, is_interrupted)? else {
            return Ok(None);
        };
        if entry.level < level {
            continue;
        }
        if entry.vector.len() != query.len() {
            return Ok(None);
        }
        if state.visit(*owner_id)? {
            let item = VectorQueueItem {
                owner_id: *owner_id,
                score: vector_similarity_numbers_controlled(
                    &entry.vector,
                    query,
                    similarity,
                    is_interrupted,
                )?,
            };
            state.enqueue(item)?;
        }
    }
    Ok(Some(state))
}

#[allow(
    clippy::too_many_arguments,
    reason = "one lazy HNSW candidate expansion keeps graph layer, query, search width and state explicit"
)]
fn expand_sql_candidate(
    cache: &mut SqlVectorCache<'_, '_>,
    candidate: VectorQueueItem,
    query: &[f32],
    similarity: &str,
    level: usize,
    ef: usize,
    state: &mut HnswSearchState,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<bool> {
    let Some(entry) = cache.entry(candidate.owner_id, is_interrupted)? else {
        return Ok(false);
    };
    let Some(neighbors) = entry.neighbors.get(level) else {
        return Ok(false);
    };
    for neighbor_id in neighbors {
        check_interrupted(is_interrupted)?;
        if *neighbor_id == candidate.owner_id {
            return Ok(false);
        }
        if !state.visit(*neighbor_id)? {
            continue;
        }
        let Some(neighbor) = cache.entry(*neighbor_id, is_interrupted)? else {
            return Ok(false);
        };
        if neighbor.level < level || neighbor.vector.len() != query.len() {
            return Ok(false);
        }
        let item = VectorQueueItem {
            owner_id: *neighbor_id,
            score: vector_similarity_numbers_controlled(
                &neighbor.vector,
                query,
                similarity,
                is_interrupted,
            )?,
        };
        state.keep(item, ef)?;
    }
    Ok(true)
}

#[cfg(test)]
fn hnsw_search(
    entries: &BTreeMap<i64, VectorCacheEntry>,
    entry_owner_id: i64,
    max_level: usize,
    query: &[f32],
    similarity: &str,
    ef_search: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<HnswLayerResult> {
    let mut current = entry_owner_id;
    for level in (1..=max_level).rev() {
        let result = hnsw_search_layer(
            entries,
            &[current],
            query,
            similarity,
            level,
            1,
            is_interrupted,
        )?;
        if let Some(best) = result.items.first() {
            current = best.owner_id;
        }
    }
    hnsw_search_layer(
        entries,
        &[current],
        query,
        similarity,
        0,
        ef_search.max(1),
        is_interrupted,
    )
}

fn hnsw_search_layer(
    entries: &BTreeMap<i64, VectorCacheEntry>,
    entry_points: &[i64],
    query: &[f32],
    similarity: &str,
    level: usize,
    ef: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<HnswLayerResult> {
    let ef = ef.max(1);
    let mut state = initialize_memory_search(
        entries,
        entry_points,
        query,
        similarity,
        level,
        is_interrupted,
    )?;
    while let Some(candidate) = state.candidates.pop() {
        check_interrupted(is_interrupted)?;
        if state.nearest.len() >= ef
            && state
                .nearest
                .peek()
                .is_some_and(|Reverse(worst)| candidate < *worst)
        {
            break;
        }
        let Some(entry) = entries.get(&candidate.owner_id) else {
            continue;
        };
        let Some(neighbors) = entry.neighbors.get(level) else {
            continue;
        };
        for neighbor_id in neighbors {
            check_interrupted(is_interrupted)?;
            if !state.visit(*neighbor_id)? {
                continue;
            }
            let Some(neighbor) = entries
                .get(neighbor_id)
                .filter(|entry| entry.level >= level)
            else {
                continue;
            };
            let item = VectorQueueItem {
                owner_id: *neighbor_id,
                score: vector_similarity_numbers_controlled(
                    &neighbor.vector,
                    query,
                    similarity,
                    is_interrupted,
                )?,
            };
            state.keep(item, ef)?;
        }
    }
    let items = ordered_hnsw_items(state.nearest, is_interrupted)?;
    Ok(HnswLayerResult {
        items,
        visited_count: state.visited.len(),
        _memory: None,
    })
}

fn initialize_memory_search(
    entries: &BTreeMap<i64, VectorCacheEntry>,
    entry_points: &[i64],
    query: &[f32],
    similarity: &str,
    level: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<HnswSearchState> {
    let mut visited = BTreeSet::new();
    let mut candidates = BinaryHeap::new();
    let mut nearest = BinaryHeap::<Reverse<VectorQueueItem>>::new();
    for owner_id in entry_points {
        let Some(entry) = entries.get(owner_id).filter(|entry| entry.level >= level) else {
            continue;
        };
        if !visited.insert(*owner_id) {
            continue;
        }
        let item = VectorQueueItem {
            owner_id: *owner_id,
            score: vector_similarity_numbers_controlled(
                &entry.vector,
                query,
                similarity,
                is_interrupted,
            )?,
        };
        candidates.push(item);
        nearest.push(Reverse(item));
    }
    Ok(HnswSearchState {
        visited,
        candidates,
        nearest,
        memory: None,
    })
}

fn semantic_hits_from_cache(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &VectorSearchInput<'_>,
    ordered: &[VectorQueueItem],
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(Vec<SemanticHit>, Option<MemoryReservation>)> {
    let connection = snapshot.connection_for_query();
    let membership = semantic_membership(connection, index)?;
    let property = index_properties(index)?
        .first()
        .ok_or_else(|| QueryError::internal("VECTOR Index is missing its indexed property"))?;
    let mut hits = Vec::new();
    let mut memory = reserve_hits(control.map(ExecutionControl::resources))?;
    for item in ordered {
        check_interrupted(is_interrupted)?;
        if let Some(hit) = cached_semantic_hit(
            snapshot,
            graph_view,
            index,
            input,
            (&membership, property),
            item,
            control,
        )? {
            reserve_hit_slot(&mut hits, &mut memory)?;
            hits.push(hit);
        }
    }
    Ok((hits, memory))
}

fn cached_semantic_hit(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &VectorSearchInput<'_>,
    property: (&SemanticMembership, &str),
    item: &VectorQueueItem,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<SemanticHit>> {
    let Some(entity) = cached_semantic_entity(snapshot, index, item.owner_id)? else {
        return Ok(None);
    };
    if !semantic_entity_matches(snapshot, entity, property.0)?
        || !semantic_entity_visible(snapshot, graph_view, entity, control)?
    {
        return Ok(None);
    }
    let (value, _property_memory) =
        semantic_property_controlled(snapshot, entity, property.1, control)?;
    vector_hit_controlled(index, input, entity, value, control)
}

fn cached_semantic_entity(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    owner_id: i64,
) -> QueryResult<Option<SemanticEntity>> {
    match index.target {
        IndexTarget::NodeProperties { .. } => Ok(Some(SemanticEntity::Node(owner_id))),
        IndexTarget::RelationshipProperties { .. } => Ok(snapshot
            .relationship(owner_id)?
            .map(SemanticEntity::Relationship)),
        IndexTarget::NodeLookup | IndexTarget::RelationshipLookup => {
            Err(QueryError::internal("VECTOR Index has a lookup target"))
        }
    }
}

pub(super) fn build_vector_cache_controlled(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    query_dimension: usize,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    check_interrupted(is_interrupted)?;
    ensure_vector_cache_tables(connection)?;
    let cache_key = vector_cache_key(snapshot, index, query_dimension)?;
    if vector_cache_meta(connection, &cache_key)?.is_some() {
        return Ok(());
    }
    #[cfg(feature = "test-support")]
    let build_started = std::time::Instant::now();
    invalidate_vector_cache(connection, &cache_key)?;
    let mut entries = collect_vector_cache_entries(
        connection,
        snapshot,
        index,
        query_dimension,
        is_interrupted,
        control,
    )?;
    build_hnsw_graph(index, &mut entries, is_interrupted)?;
    let result = persist_vector_cache(connection, &cache_key, &entries, is_interrupted);
    if result.is_err() {
        let _ = invalidate_vector_cache(connection, &cache_key);
    }
    #[cfg(feature = "test-support")]
    if result.is_ok() {
        crate::performance::record_vector_cache_build(build_started.elapsed().as_micros());
    }
    result
}

fn collect_vector_cache_entries(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    query_dimension: usize,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Vec<VectorCacheEntry>> {
    let property = index_properties(index)?
        .first()
        .ok_or_else(|| QueryError::internal("VECTOR Index is missing its indexed property"))?;
    let mut entries = Vec::new();
    visit_indexed_entities(
        connection,
        snapshot,
        index,
        None,
        is_interrupted,
        control,
        |entity| {
            check_interrupted(is_interrupted)?;
            let (value, _property_memory) =
                semantic_property_controlled(snapshot, entity, property, control)?;
            if let Some(entry) = vector_cache_entry(index, entity_id(entity), value, control)?
                && entry.vector.len() == query_dimension
            {
                entries.push(entry);
            }
            Ok(())
        },
    )?;
    entries.sort_by_key(|entry| entry.owner_id);
    Ok(entries)
}

fn vector_cache_entry(
    index: &IndexDefinition,
    owner_id: i64,
    value: Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<VectorCacheEntry>> {
    let (vector, _vector_memory) = try_vector_numbers_controlled(&value, control)?;
    let Some(vector) = vector else {
        return Ok(None);
    };
    if !stored_vector_valid_controlled(&vector, index, control)? {
        return Ok(None);
    }
    Ok(Some(VectorCacheEntry {
        owner_id,
        vector,
        level: 0,
        neighbors: vec![Vec::new()],
    }))
}

fn build_hnsw_graph(
    definition: &IndexDefinition,
    entries: &mut Vec<VectorCacheEntry>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let Some(IndexConfiguration::Vector {
        similarity_function,
        hnsw_m,
        hnsw_ef_construction,
        ..
    }) = &definition.configuration
    else {
        return Err(QueryError::internal(
            "VECTOR Index has no HNSW configuration",
        ));
    };
    let maximum = usize::try_from(*hnsw_m)
        .map_err(|_| QueryError::internal("VECTOR Index HNSW M is too large"))?
        .max(1);
    let ef_construction = usize::try_from(*hnsw_ef_construction)
        .map_err(|_| QueryError::internal("VECTOR Index HNSW ef_construction is too large"))?
        .max(maximum);
    build_hnsw_graph_with_parameters(
        similarity_function,
        maximum,
        ef_construction,
        entries,
        is_interrupted,
    )
}

fn build_hnsw_graph_with_parameters(
    similarity_function: &str,
    maximum: usize,
    ef_construction: usize,
    entries: &mut Vec<VectorCacheEntry>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    entries.sort_by_key(|entry| entry.owner_id);
    initialize_hnsw_entries(entries, maximum, is_interrupted)?;
    if maximum == 1 {
        return connect_single_neighbor_hnsw_layers(entries, is_interrupted);
    }
    let source = std::mem::take(entries);
    let mut graph = BTreeMap::<i64, VectorCacheEntry>::new();
    let mut entry_owner_id = None;
    let mut max_level = 0usize;

    for mut entry in source {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if graph.is_empty() {
            max_level = entry.level;
            entry_owner_id = Some(entry.owner_id);
            graph.insert(entry.owner_id, entry);
            continue;
        }

        let current_entry = entry_owner_id.ok_or_else(|| {
            QueryError::internal("HNSW graph is missing its entry point during construction")
        })?;
        let selected = select_hnsw_neighbors_for_new_entry(
            &graph,
            current_entry,
            max_level,
            &entry,
            similarity_function,
            maximum,
            ef_construction,
            is_interrupted,
        )?;
        entry.neighbors = selected;
        let owner_id = entry.owner_id;
        let level = entry.level;
        graph.insert(owner_id, entry);
        connect_hnsw_entry(
            &mut graph,
            owner_id,
            similarity_function,
            maximum,
            is_interrupted,
        )?;
        if level > max_level {
            max_level = level;
            entry_owner_id = Some(owner_id);
        }
    }

    let mut completed = Vec::with_capacity(graph.len());
    for entry in graph.into_values() {
        check_interrupted(is_interrupted)?;
        completed.push(entry);
    }
    *entries = completed;
    Ok(())
}

fn initialize_hnsw_entries(
    entries: &mut [VectorCacheEntry],
    maximum_connections: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    for entry in entries {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        entry.level = deterministic_hnsw_level(entry.owner_id, maximum_connections);
        entry.neighbors = vec![Vec::new(); entry.level.saturating_add(1)];
    }
    Ok(())
}

fn connect_single_neighbor_hnsw_layers(
    entries: &mut [VectorCacheEntry],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let max_level = entries.iter().map(|entry| entry.level).max().unwrap_or(0);
    for level in 0..=max_level {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        let mut indices = Vec::new();
        for (index, entry) in entries.iter().enumerate() {
            check_interrupted(is_interrupted)?;
            if entry.level >= level {
                indices.push(index);
            }
        }
        if indices.len() <= 1 {
            continue;
        }
        let mut owners = Vec::with_capacity(indices.len());
        for index in &indices {
            check_interrupted(is_interrupted)?;
            owners.push(entries[*index].owner_id);
        }
        for (position, index) in indices.into_iter().enumerate() {
            check_interrupted(is_interrupted)?;
            entries[index].neighbors[level] = vec![owners[(position + 1) % owners.len()]];
        }
    }
    Ok(())
}

fn deterministic_hnsw_level(owner_id: i64, maximum_connections: usize) -> usize {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_HNSW_LEVEL_V1");
    hasher.update(&owner_id.to_le_bytes());
    let digest = hasher.finalize();
    let mut bytes = [0u8; 8];
    bytes.copy_from_slice(&digest.as_bytes()[..8]);
    let mut value = u64::from_le_bytes(bytes);
    let base = u64::try_from(maximum_connections.max(2)).unwrap_or(u64::MAX);
    let mut level = 0usize;
    while level < 32 && value.is_multiple_of(base) {
        level += 1;
        value /= base;
    }
    level
}

#[allow(
    clippy::too_many_arguments,
    reason = "HNSW insertion keeps graph/search parameters explicit so construction and query semantics share one layer-search primitive"
)]
fn select_hnsw_neighbors_for_new_entry(
    graph: &BTreeMap<i64, VectorCacheEntry>,
    entry_owner_id: i64,
    max_level: usize,
    entry: &VectorCacheEntry,
    similarity: &str,
    maximum: usize,
    ef_construction: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<Vec<i64>>> {
    let mut selected = vec![Vec::new(); entry.level.saturating_add(1)];
    let mut current = entry_owner_id;
    for level in ((entry.level.saturating_add(1))..=max_level).rev() {
        let result = hnsw_search_layer(
            graph,
            &[current],
            &entry.vector,
            similarity,
            level,
            1,
            is_interrupted,
        )?;
        if let Some(best) = result.items.first() {
            current = best.owner_id;
        }
    }
    for level in (0..=entry.level.min(max_level)).rev() {
        let result = hnsw_search_layer(
            graph,
            &[current],
            &entry.vector,
            similarity,
            level,
            ef_construction.min(graph.len()).max(1),
            is_interrupted,
        )?;
        let mut owners = Vec::with_capacity(result.items.len().min(maximum));
        for item in result.items.iter().take(maximum) {
            check_interrupted(is_interrupted)?;
            owners.push(item.owner_id);
        }
        selected[level] = owners;
        if let Some(best) = result.items.first() {
            current = best.owner_id;
        }
    }
    Ok(selected)
}

fn connect_hnsw_entry(
    graph: &mut BTreeMap<i64, VectorCacheEntry>,
    owner_id: i64,
    similarity: &str,
    maximum: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let entry = graph
        .get(&owner_id)
        .ok_or_else(|| QueryError::internal("new HNSW entry disappeared during construction"))?;
    let mut selected = Vec::with_capacity(entry.neighbors.len());
    for layer in &entry.neighbors {
        let mut copied = Vec::with_capacity(layer.len());
        for neighbors in layer.chunks(256) {
            check_interrupted(is_interrupted)?;
            copied.extend_from_slice(neighbors);
        }
        selected.push(copied);
    }
    for (level, neighbors) in selected.into_iter().enumerate() {
        let layer_maximum = if level == 0 {
            maximum.saturating_mul(2)
        } else {
            maximum
        };
        for neighbor_id in neighbors {
            connect_hnsw_neighbor(
                graph,
                owner_id,
                neighbor_id,
                level,
                similarity,
                layer_maximum,
                is_interrupted,
            )?;
        }
    }
    Ok(())
}

fn connect_hnsw_neighbor(
    graph: &mut BTreeMap<i64, VectorCacheEntry>,
    owner_id: i64,
    neighbor_id: i64,
    level: usize,
    similarity: &str,
    layer_maximum: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    check_interrupted(is_interrupted)?;
    let neighbor = graph
        .get_mut(&neighbor_id)
        .ok_or_else(|| QueryError::internal("HNSW selected a missing neighbor"))?;
    let layer = neighbor
        .neighbors
        .get_mut(level)
        .ok_or_else(|| QueryError::internal("HNSW selected a neighbor below the layer"))?;
    let mut contains_owner = false;
    for neighbors in layer.chunks(256) {
        check_interrupted(is_interrupted)?;
        contains_owner |= neighbors.contains(&owner_id);
    }
    if !contains_owner {
        layer.push(owner_id);
    }
    let pruned = prune_hnsw_neighbors(
        graph,
        neighbor_id,
        level,
        similarity,
        layer_maximum,
        is_interrupted,
    )?;
    let layer = graph
        .get_mut(&neighbor_id)
        .and_then(|entry| entry.neighbors.get_mut(level))
        .ok_or_else(|| QueryError::internal("HNSW neighbor layer disappeared"))?;
    layer.clear();
    for neighbors in pruned.chunks(256) {
        check_interrupted(is_interrupted)?;
        layer.extend_from_slice(neighbors);
    }
    Ok(())
}

fn prune_hnsw_neighbors(
    graph: &BTreeMap<i64, VectorCacheEntry>,
    owner_id: i64,
    level: usize,
    similarity: &str,
    maximum: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<i64>> {
    let entry = graph
        .get(&owner_id)
        .ok_or_else(|| QueryError::internal("HNSW prune owner is missing"))?;
    let neighbors = entry
        .neighbors
        .get(level)
        .ok_or_else(|| QueryError::internal("HNSW prune layer is missing"))?;
    let mut scored = Vec::with_capacity(neighbors.len());
    for neighbor_id in neighbors {
        check_interrupted(is_interrupted)?;
        let neighbor = graph
            .get(neighbor_id)
            .ok_or_else(|| QueryError::internal("HNSW prune neighbor is missing"))?;
        scored.push((
            *neighbor_id,
            vector_similarity_numbers_controlled(
                &entry.vector,
                &neighbor.vector,
                similarity,
                is_interrupted,
            )?,
        ));
    }
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    check_interrupted(is_interrupted)?;
    let mut owners = Vec::with_capacity(scored.len().min(maximum));
    for (neighbor_id, _) in scored.into_iter().take(maximum) {
        check_interrupted(is_interrupted)?;
        owners.push(neighbor_id);
    }
    Ok(owners)
}

#[cfg(test)]
mod tests;
