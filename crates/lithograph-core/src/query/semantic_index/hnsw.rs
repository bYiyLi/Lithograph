use std::cmp::{Ordering, Reverse};
use std::collections::{BTreeMap, BTreeSet, BinaryHeap};

use rusqlite::Connection;

use super::*;

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
}

impl SqlVectorCache<'_, '_> {
    fn entry(&mut self, owner_id: i64) -> QueryResult<Option<VectorCacheEntry>> {
        if let Some(entry) = self.loaded.get(&owner_id) {
            return Ok(Some(entry.clone()));
        }
        #[cfg(feature = "test-support")]
        let load_started = std::time::Instant::now();
        let row = self
            .connection
            .query_row(
                "SELECT vector_json, level, neighbors_json \
                 FROM temp._lithograph_vector_cache \
                 WHERE cache_key = ?1 AND owner_id = ?2",
                rusqlite::params![self.cache_key, owner_id],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((vector_json, level, neighbors_json)) = row else {
            return Ok(None);
        };
        let Ok(vector) = serde_json::from_str::<Vec<f32>>(&vector_json) else {
            return Ok(None);
        };
        let Ok(neighbors) = serde_json::from_str::<Vec<Vec<i64>>>(&neighbors_json) else {
            return Ok(None);
        };
        let Ok(level) = usize::try_from(level) else {
            return Ok(None);
        };
        if vector.is_empty() || neighbors.len() != level.saturating_add(1) {
            return Ok(None);
        }
        let entry = VectorCacheEntry {
            owner_id,
            vector,
            level,
            neighbors,
        };
        self.loaded.insert(owner_id, entry.clone());
        #[cfg(feature = "test-support")]
        crate::performance::record_vector_cache_entry_load(load_started.elapsed().as_micros());
        Ok(Some(entry))
    }
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
}

struct SqlSearchState {
    visited: BTreeSet<i64>,
    candidates: BinaryHeap<VectorQueueItem>,
    nearest: BinaryHeap<Reverse<VectorQueueItem>>,
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

fn vector_cache_meta(
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

fn invalidate_vector_cache(connection: &Connection, cache_key: &str) -> QueryResult<()> {
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

pub(super) fn query_vector_cache(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    query: &Value,
    candidate_limit: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<VectorCandidateResult>> {
    let connection = snapshot.connection_for_query();
    ensure_vector_cache_tables(connection)?;
    let query_vector = vector_numbers(query)?;
    let cache_key = vector_cache_key(snapshot, index, query_vector.len())?;
    let Some(meta) = vector_cache_meta(connection, &cache_key)? else {
        return Ok(None);
    };
    if meta.entry_count == 0 {
        if meta.entry_owner_id.is_some() || meta.max_level != 0 {
            invalidate_vector_cache(connection, &cache_key)?;
            return Ok(None);
        }
        return Ok(Some(VectorCandidateResult {
            hits: Vec::new(),
            exhaustive: true,
        }));
    }
    let Some(mut cache) = open_sql_vector_cache(connection, &cache_key, meta, query_vector.len())?
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
) -> QueryResult<Option<SqlVectorCache<'connection, 'key>>> {
    let Some(entry_owner_id) = meta.entry_owner_id else {
        invalidate_vector_cache(connection, cache_key)?;
        return Ok(None);
    };
    let mut cache = SqlVectorCache {
        connection,
        cache_key,
        loaded: BTreeMap::new(),
    };
    let Some(entry) = cache.entry(entry_owner_id)? else {
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
        let mut hits = semantic_hits_from_cache(snapshot, graph_view, index, input, &layer.items)?;
        sort_semantic_hits(&mut hits);
        if hits.len() >= desired || exhaustive {
            return Ok(Some(VectorCandidateResult { hits, exhaustive }));
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
    if entries.is_empty() {
        return meta.entry_count == 0 && meta.entry_owner_id.is_none() && meta.max_level == 0;
    }
    let Some(entry_owner_id) = meta.entry_owner_id else {
        return false;
    };
    let Some(entry) = entries.get(&entry_owner_id) else {
        return false;
    };
    if entry.level != meta.max_level || entries.values().any(|item| item.level > meta.max_level) {
        return false;
    }
    if !entries.values().all(|item| {
        item.neighbors.iter().enumerate().all(|(level, neighbors)| {
            neighbors.iter().all(|neighbor_id| {
                *neighbor_id != item.owner_id
                    && entries
                        .get(neighbor_id)
                        .is_some_and(|neighbor| neighbor.level >= level)
            })
        })
    }) {
        return false;
    }
    let mut reachable = BTreeSet::from([entry_owner_id]);
    let mut pending = vec![entry_owner_id];
    while let Some(owner_id) = pending.pop() {
        let Some(neighbors) = entries
            .get(&owner_id)
            .and_then(|entry| entry.neighbors.first())
        else {
            return false;
        };
        for neighbor_id in neighbors {
            if reachable.insert(*neighbor_id) {
                pending.push(*neighbor_id);
            }
        }
    }
    reachable.len() == entries.len()
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
    let Some(mut state) = initialize_sql_search(cache, entry_points, query, similarity, level)?
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
        if !expand_sql_candidate(cache, candidate, query, similarity, level, ef, &mut state)? {
            return Ok(None);
        }
    }
    let mut items = state
        .nearest
        .into_iter()
        .map(|Reverse(item)| item)
        .collect::<Vec<_>>();
    items.sort_by(|left, right| right.cmp(left));
    Ok(Some(HnswLayerResult {
        items,
        visited_count: state.visited.len(),
    }))
}

fn initialize_sql_search(
    cache: &mut SqlVectorCache<'_, '_>,
    entry_points: &[i64],
    query: &[f32],
    similarity: &str,
    level: usize,
) -> QueryResult<Option<SqlSearchState>> {
    let mut state = SqlSearchState {
        visited: BTreeSet::new(),
        candidates: BinaryHeap::new(),
        nearest: BinaryHeap::new(),
    };
    for owner_id in entry_points {
        let Some(entry) = cache.entry(*owner_id)? else {
            return Ok(None);
        };
        if entry.level < level {
            continue;
        }
        if entry.vector.len() != query.len() {
            return Ok(None);
        }
        if state.visited.insert(*owner_id) {
            let item = VectorQueueItem {
                owner_id: *owner_id,
                score: vector_similarity_numbers(&entry.vector, query, similarity)?,
            };
            state.candidates.push(item);
            state.nearest.push(Reverse(item));
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
    state: &mut SqlSearchState,
) -> QueryResult<bool> {
    let Some(entry) = cache.entry(candidate.owner_id)? else {
        return Ok(false);
    };
    let Some(neighbors) = entry.neighbors.get(level) else {
        return Ok(false);
    };
    for neighbor_id in neighbors {
        if *neighbor_id == candidate.owner_id {
            return Ok(false);
        }
        if !state.visited.insert(*neighbor_id) {
            continue;
        }
        let Some(neighbor) = cache.entry(*neighbor_id)? else {
            return Ok(false);
        };
        if neighbor.level < level || neighbor.vector.len() != query.len() {
            return Ok(false);
        }
        let item = VectorQueueItem {
            owner_id: *neighbor_id,
            score: vector_similarity_numbers(&neighbor.vector, query, similarity)?,
        };
        let should_keep = state.nearest.len() < ef
            || state
                .nearest
                .peek()
                .is_none_or(|Reverse(worst)| item > *worst);
        if should_keep {
            state.candidates.push(item);
            state.nearest.push(Reverse(item));
            if state.nearest.len() > ef {
                state.nearest.pop();
            }
        }
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
            score: vector_similarity_numbers(&entry.vector, query, similarity)?,
        };
        candidates.push(item);
        nearest.push(Reverse(item));
    }
    while let Some(candidate) = candidates.pop() {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if nearest.len() >= ef
            && nearest
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
            if !visited.insert(*neighbor_id) {
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
                score: vector_similarity_numbers(&neighbor.vector, query, similarity)?,
            };
            let should_keep =
                nearest.len() < ef || nearest.peek().is_none_or(|Reverse(worst)| item > *worst);
            if should_keep {
                candidates.push(item);
                nearest.push(Reverse(item));
                if nearest.len() > ef {
                    nearest.pop();
                }
            }
        }
    }
    let mut items = nearest
        .into_iter()
        .map(|Reverse(item)| item)
        .collect::<Vec<_>>();
    items.sort_by(|left, right| right.cmp(left));
    Ok(HnswLayerResult {
        items,
        visited_count: visited.len(),
    })
}

fn semantic_hits_from_cache(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &VectorSearchInput<'_>,
    ordered: &[VectorQueueItem],
) -> QueryResult<Vec<SemanticHit>> {
    let connection = snapshot.connection_for_query();
    let membership = semantic_membership(connection, index)?;
    let property = index_properties(index)?
        .first()
        .ok_or_else(|| QueryError::internal("VECTOR Index is missing its indexed property"))?;
    let mut hits = Vec::new();
    for item in ordered {
        let entity = match index.target {
            IndexTarget::NodeProperties { .. } => SemanticEntity::Node(item.owner_id),
            IndexTarget::RelationshipProperties { .. } => {
                let Some(relationship) = snapshot.relationship(item.owner_id)? else {
                    continue;
                };
                SemanticEntity::Relationship(relationship)
            }
            IndexTarget::NodeLookup | IndexTarget::RelationshipLookup => {
                return Err(QueryError::internal("VECTOR Index has a lookup target"));
            }
        };
        if !semantic_entity_matches(snapshot, entity, &membership)?
            || !semantic_entity_visible(snapshot, graph_view, entity)?
        {
            continue;
        }
        let value = semantic_property(snapshot, entity, property)?;
        if let Some(hit) = vector_hit(index, input, entity, value)? {
            hits.push(hit);
        }
    }
    Ok(hits)
}

pub(super) fn build_vector_cache(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    query_dimension: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    ensure_vector_cache_tables(connection)?;
    let cache_key = vector_cache_key(snapshot, index, query_dimension)?;
    if vector_cache_meta(connection, &cache_key)?.is_some() {
        return Ok(());
    }
    #[cfg(feature = "test-support")]
    let build_started = std::time::Instant::now();
    invalidate_vector_cache(connection, &cache_key)?;
    let mut entries =
        collect_vector_cache_entries(connection, snapshot, index, query_dimension, is_interrupted)?;
    build_hnsw_graph(index, &mut entries, is_interrupted)?;
    let result = persist_vector_cache(connection, &cache_key, &entries);
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
        |entity| {
            let value = semantic_property(snapshot, entity, property)?;
            if let Some(entry) = vector_cache_entry(index, entity_id(entity), value)?
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
) -> QueryResult<Option<VectorCacheEntry>> {
    let Some(vector) = try_vector_numbers(&value)? else {
        return Ok(None);
    };
    if !stored_vector_valid(&vector, index)? {
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

    *entries = graph.into_values().collect();
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
        let indices = entries
            .iter()
            .enumerate()
            .filter_map(|(index, entry)| (entry.level >= level).then_some(index))
            .collect::<Vec<_>>();
        if indices.len() <= 1 {
            continue;
        }
        let owners = indices
            .iter()
            .map(|index| entries[*index].owner_id)
            .collect::<Vec<_>>();
        for (position, index) in indices.into_iter().enumerate() {
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
        selected[level] = result
            .items
            .iter()
            .take(maximum)
            .map(|item| item.owner_id)
            .collect();
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
    let selected = graph
        .get(&owner_id)
        .map(|entry| entry.neighbors.clone())
        .ok_or_else(|| QueryError::internal("new HNSW entry disappeared during construction"))?;
    for (level, neighbors) in selected.into_iter().enumerate() {
        let layer_maximum = if level == 0 {
            maximum.saturating_mul(2)
        } else {
            maximum
        };
        for neighbor_id in neighbors {
            if is_interrupted() {
                return Err(QueryError::interrupted());
            }
            let Some(neighbor) = graph.get_mut(&neighbor_id) else {
                return Err(QueryError::internal("HNSW selected a missing neighbor"));
            };
            let Some(layer) = neighbor.neighbors.get_mut(level) else {
                return Err(QueryError::internal(
                    "HNSW selected a neighbor below the layer",
                ));
            };
            if !layer.contains(&owner_id) {
                layer.push(owner_id);
            }
            let pruned =
                prune_hnsw_neighbors(graph, neighbor_id, level, similarity, layer_maximum)?;
            graph
                .get_mut(&neighbor_id)
                .and_then(|entry| entry.neighbors.get_mut(level))
                .ok_or_else(|| QueryError::internal("HNSW neighbor layer disappeared"))?
                .clone_from(&pruned);
        }
    }
    Ok(())
}

fn prune_hnsw_neighbors(
    graph: &BTreeMap<i64, VectorCacheEntry>,
    owner_id: i64,
    level: usize,
    similarity: &str,
    maximum: usize,
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
        let neighbor = graph
            .get(neighbor_id)
            .ok_or_else(|| QueryError::internal("HNSW prune neighbor is missing"))?;
        scored.push((
            *neighbor_id,
            vector_similarity_numbers(&entry.vector, &neighbor.vector, similarity)?,
        ));
    }
    scored.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    Ok(scored
        .into_iter()
        .take(maximum)
        .map(|(neighbor_id, _)| neighbor_id)
        .collect())
}

fn persist_vector_cache(
    connection: &Connection,
    cache_key: &str,
    entries: &[VectorCacheEntry],
) -> QueryResult<()> {
    for entry in entries {
        let vector_json = serde_json::to_string(&entry.vector).map_err(|error| {
            QueryError::internal(format!("failed to encode HNSW vector: {error}"))
        })?;
        let neighbors_json = serde_json::to_string(&entry.neighbors).map_err(|error| {
            QueryError::internal(format!("failed to encode HNSW neighbors: {error}"))
        })?;
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
    connection.execute(
        "INSERT OR REPLACE INTO temp._lithograph_vector_cache_meta(cache_key, entry_owner_id, max_level, entry_count, complete) VALUES(?1, ?2, ?3, ?4, 1)",
        rusqlite::params![cache_key, entry_owner_id, max_level, entry_count],
    )?;
    Ok(())
}

#[cfg(test)]
mod tests;
