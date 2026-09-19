use std::collections::{BTreeMap, BTreeSet};

use lithograph_embedding_provider::{
    ProviderError, ProviderErrorKind, RegisteredEmbeddingProvider,
};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::cypher::Value;
use crate::storage::{
    self, EmbeddingCacheEntry, HashId, IndexConfiguration, IndexDefinition, IndexTarget,
    SchemaState, Snapshot, StandardIndexKind,
};

use super::graph::ResolvedGraphView;
use super::semantic_index::{
    SemanticEntity, SemanticHit, SemanticMembership, build_managed_hnsw_cache, entity_id,
    has_finite_nonzero_norm, invalidate_managed_hnsw_cache, query_managed_hnsw_cache,
    semantic_cache_digest, semantic_entity_matches, semantic_entity_visible, semantic_membership,
    semantic_property, sort_semantic_hits, vector_similarity_numbers, visit_indexed_entities,
};
use super::{QueryError, QueryErrorKind, QueryResult};

const QUERY_SOURCE_BATCH_ENTITIES: usize = 1_024;
const QUERY_SOURCE_BATCH_TEXTS: usize = 64;
const REBUILD_TEXT_BATCH: usize = 64;
const REBUILD_PUBLISH_BATCH: usize = 256;
type ManagedHnswEntries = Vec<(i64, Vec<f32>)>;
type RebuildTextEntry = (Vec<u8>, String);
type RebuildCacheResolution = (Vec<RebuildTextEntry>, u64);

pub(crate) struct ManagedQueryInput<'a> {
    pub(crate) relationship_query: bool,
    pub(crate) query: &'a str,
    pub(crate) skip: usize,
    pub(crate) limit: usize,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct ManagedRebuildOutcome {
    pub(crate) commit: HashId,
    pub(crate) indexed_entities: u64,
    pub(crate) embedded_texts: u64,
    pub(crate) cache_hits: u64,
}

struct ManagedRuntime<'connection> {
    provider: RegisteredEmbeddingProvider<'connection>,
    config_json: Vec<u8>,
    dimensions: usize,
    similarity: String,
    space_hash: HashId,
}

pub(crate) fn semantic_definition(
    index: &IndexDefinition,
) -> Option<(&str, &serde_json::Value, u64, &str)> {
    let IndexConfiguration::Semantic {
        provider,
        provider_config,
        dimensions,
        similarity_function,
    } = index.configuration.as_ref()?
    else {
        return None;
    };
    (index.kind == StandardIndexKind::Semantic).then_some((
        provider,
        provider_config,
        *dimensions,
        similarity_function,
    ))
}

pub(crate) fn validate_definition(
    connection: &Connection,
    definition: &IndexDefinition,
) -> QueryResult<()> {
    let Some((provider_name, provider_config, dimensions, _)) = semantic_definition(definition)
    else {
        return Ok(());
    };
    let provider = RegisteredEmbeddingProvider::lookup(connection, provider_name)
        .map_err(map_provider_error)?;
    let config = serde_json::to_vec(provider_config).map_err(|error| {
        QueryError::internal(format!("failed to encode providerConfig: {error}"))
    })?;
    provider
        .validate(&config, dimensions as usize)
        .map_err(map_provider_error)
}

pub(crate) fn validate_transition(
    connection: &Connection,
    previous: &SchemaState,
    next: &SchemaState,
) -> QueryResult<()> {
    for (name, definition) in &next.indexes {
        if definition.kind != StandardIndexKind::Semantic {
            continue;
        }
        if previous.indexes.get(name) == Some(definition) {
            continue;
        }
        storage::require_embedding_cache_format(connection)?;
        validate_definition(connection, definition)?;
    }
    Ok(())
}

pub(crate) fn query(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &ManagedQueryInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<SemanticHit>> {
    validate_target(index, input.relationship_query)?;
    let runtime = resolve_runtime(connection, index)?;
    if input.limit == 0 {
        return Ok(Vec::new());
    }
    let needed = input
        .skip
        .checked_add(input.limit)
        .ok_or_else(|| QueryError::invalid_argument("semantic skip + limit is too large"))?;
    let query_vector = resolve_query_embedding(connection, &runtime, input.query, is_interrupted)?;
    let cache_key = managed_hnsw_cache_key(snapshot, index, runtime.space_hash)?;
    let membership = semantic_membership(connection, index)?;
    let property = source_property(index)?.to_owned();
    if let Some(mut cached) = query_managed_hnsw_cache(
        connection,
        &cache_key,
        &query_vector,
        &runtime.similarity,
        needed,
        is_interrupted,
        |owner_id| {
            resolve_cached_entity(
                snapshot,
                graph_view,
                index,
                &membership,
                &property,
                owner_id,
            )
        },
    )? {
        sort_semantic_hits(&mut cached.hits);
        cached.hits.truncate(needed);
        return Ok(cached
            .hits
            .into_iter()
            .skip(input.skip)
            .take(input.limit)
            .collect());
    }
    let collect_hnsw = graph_view.is_full_graph();
    let (mut hits, hnsw_entries) = collect_query_hits(
        connection,
        snapshot,
        graph_view,
        index,
        &runtime,
        &query_vector,
        needed,
        collect_hnsw,
        is_interrupted,
    )?;
    sort_semantic_hits(&mut hits);
    hits.truncate(needed);
    if collect_hnsw {
        build_managed_hnsw_cache(
            connection,
            &cache_key,
            &runtime.similarity,
            hnsw_entries,
            is_interrupted,
        )?;
    }
    Ok(hits
        .into_iter()
        .skip(input.skip)
        .take(input.limit)
        .collect())
}

fn managed_hnsw_cache_key(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    space_hash: HashId,
) -> QueryResult<String> {
    let base = semantic_cache_digest(snapshot, index, b"LITHOGRAPH_MANAGED_SEMANTIC_HNSW_V1")?;
    Ok(format!("{base}:{}", space_hash.to_hex()))
}

fn resolve_cached_entity(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    membership: &SemanticMembership,
    property: &str,
    owner_id: i64,
) -> QueryResult<Option<SemanticEntity>> {
    let entity = match index.target {
        IndexTarget::NodeProperties { .. } => {
            if !snapshot.node_exists(owner_id)? {
                return Ok(None);
            }
            SemanticEntity::Node(owner_id)
        }
        IndexTarget::RelationshipProperties { .. } => {
            let Some(relationship) = snapshot.relationship(owner_id)? else {
                return Ok(None);
            };
            SemanticEntity::Relationship(relationship)
        }
        IndexTarget::NodeLookup | IndexTarget::RelationshipLookup => {
            return Err(QueryError::internal("Semantic Index has a lookup target"));
        }
    };
    if !semantic_entity_matches(snapshot, entity, membership)?
        || !semantic_entity_visible(snapshot, graph_view, entity)?
        || !matches!(
            semantic_property(snapshot, entity, property)?,
            Value::String(_)
        )
    {
        return Ok(None);
    }
    Ok(Some(entity))
}

fn resolve_query_embedding(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    query: &str,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<f32>> {
    resolve_embeddings(connection, runtime, &[query.to_owned()], is_interrupted)?
        .remove(query)
        .ok_or_else(|| QueryError::internal("semantic query embedding is missing"))
}

#[allow(
    clippy::too_many_arguments,
    reason = "query hit collection receives immutable execution context plus bounded mutable batch state"
)]
fn collect_query_hits(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    runtime: &ManagedRuntime<'_>,
    query_vector: &[f32],
    needed: usize,
    collect_hnsw: bool,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<(Vec<SemanticHit>, ManagedHnswEntries)> {
    let property = source_property(index)?.to_owned();
    let mut pending = Vec::<(SemanticEntity, String)>::new();
    let mut pending_texts = BTreeSet::<String>::new();
    let mut hits = Vec::<SemanticHit>::new();
    let mut hnsw_entries = ManagedHnswEntries::new();
    visit_indexed_entities(
        connection,
        snapshot,
        index,
        Some(graph_view),
        is_interrupted,
        |entity| {
            let Value::String(text) = semantic_property(snapshot, entity, &property)? else {
                return Ok(());
            };
            pending_texts.insert(text.clone());
            pending.push((entity, text));
            if pending.len() >= QUERY_SOURCE_BATCH_ENTITIES
                || pending_texts.len() >= QUERY_SOURCE_BATCH_TEXTS
            {
                flush_query_source_batch(
                    connection,
                    runtime,
                    query_vector,
                    &mut pending,
                    &mut pending_texts,
                    &mut hits,
                    &mut hnsw_entries,
                    needed,
                    collect_hnsw,
                    is_interrupted,
                )?;
            }
            Ok(())
        },
    )?;
    flush_query_source_batch(
        connection,
        runtime,
        query_vector,
        &mut pending,
        &mut pending_texts,
        &mut hits,
        &mut hnsw_entries,
        needed,
        collect_hnsw,
        is_interrupted,
    )?;
    Ok((hits, hnsw_entries))
}

pub(crate) fn rebuild(
    connection: &Connection,
    index_name: &str,
    version: &str,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<ManagedRebuildOutcome> {
    storage::require_embedding_cache_format(connection)?;
    let (commit, index) = resolve_rebuild_target(connection, index_name, version)?;
    let runtime = resolve_runtime(connection, &index)?;
    let snapshot = Snapshot::resolve(connection, commit)?;

    create_rebuild_stage(connection)?;
    let result = rebuild_staged(
        connection,
        index_name,
        commit,
        &index,
        &snapshot,
        &runtime,
        is_interrupted,
    );
    finish_rebuild_stage(connection, result)
}

fn resolve_rebuild_target(
    connection: &Connection,
    index_name: &str,
    version: &str,
) -> QueryResult<(HashId, IndexDefinition)> {
    let commit = storage::resolve_version_descriptor(connection, version)?;
    let schema = SchemaState::load(connection, commit)?;
    let index = schema.indexes.get(index_name).cloned().ok_or_else(|| {
        QueryError::invalid_argument(format!(
            "Semantic Index {index_name:?} was not found at {version}"
        ))
    })?;
    if index.kind != StandardIndexKind::Semantic {
        return Err(QueryError::invalid_argument(format!(
            "Index {index_name:?} is not a Semantic Index"
        )));
    }
    Ok((commit, index))
}

#[allow(
    clippy::too_many_arguments,
    reason = "rebuild staging keeps the pinned version, definition, snapshot, provider runtime and cancellation explicit"
)]
fn rebuild_staged(
    connection: &Connection,
    index_name: &str,
    commit: HashId,
    index: &IndexDefinition,
    snapshot: &Snapshot<'_>,
    runtime: &ManagedRuntime<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<ManagedRebuildOutcome> {
    let indexed_entities = stage_rebuild_sources(connection, snapshot, index, is_interrupted)?;
    let (embedded_texts, cache_hits) = fill_rebuild_stage(connection, runtime, is_interrupted)?;
    validate_rebuild_definition(connection, commit, index_name, index)?;
    let cache_key = managed_hnsw_cache_key(snapshot, index, runtime.space_hash)?;
    let hnsw_entries = collect_rebuild_hnsw_entries(
        connection,
        snapshot,
        index,
        runtime.dimensions,
        is_interrupted,
    )?;
    let built_hnsw = build_managed_hnsw_cache(
        connection,
        &cache_key,
        &runtime.similarity,
        hnsw_entries,
        is_interrupted,
    )?;
    let publish = publish_rebuild_stage(
        connection,
        runtime,
        index,
        commit,
        index_name,
        is_interrupted,
    );
    if publish.is_err() && built_hnsw {
        let _ = invalidate_managed_hnsw_cache(connection, &cache_key);
    }
    publish?;
    Ok(ManagedRebuildOutcome {
        commit,
        indexed_entities,
        embedded_texts,
        cache_hits,
    })
}

fn collect_rebuild_hnsw_entries(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    dimensions: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<(i64, Vec<f32>)>> {
    let mut entries = Vec::new();
    visit_rebuild_sources(
        connection,
        snapshot,
        index,
        is_interrupted,
        |entity, text| {
            entries.push((
                entity_id(entity),
                rebuild_stage_vector(connection, text, dimensions)?,
            ));
            Ok(())
        },
    )?;
    Ok(entries)
}

fn rebuild_stage_vector(
    connection: &Connection,
    text: &str,
    dimensions: usize,
) -> QueryResult<Vec<f32>> {
    let hash = storage::embedding_text_hash(text);
    let (stored_text, blob) = connection
        .query_row(
            "SELECT text_value,vector_blob FROM temp._lithograph_semantic_rebuild_stage WHERE text_hash=?1",
            [hash.as_bytes().as_slice()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
        .ok_or_else(|| QueryError::internal("semantic rebuild TEMP vector is missing"))?;
    if stored_text != text {
        return Err(QueryError::new(
            QueryErrorKind::Storage,
            "semantic rebuild detected a text hash collision",
        ));
    }
    decode_stage_vector(&blob, dimensions)
}

fn stage_rebuild_sources(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<u64> {
    let mut indexed_entities = 0_u64;
    visit_rebuild_sources(connection, snapshot, index, is_interrupted, |_, text| {
        indexed_entities = indexed_entities.saturating_add(1);
        stage_rebuild_text(connection, text)
    })?;
    Ok(indexed_entities)
}

fn visit_rebuild_sources(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
    mut visit: impl FnMut(SemanticEntity, &str) -> QueryResult<()>,
) -> QueryResult<()> {
    let property = source_property(index)?.to_owned();
    visit_indexed_entities(
        connection,
        snapshot,
        index,
        None,
        is_interrupted,
        |entity| {
            if is_interrupted() {
                return Err(QueryError::interrupted());
            }
            let Value::String(text) = semantic_property(snapshot, entity, &property)? else {
                return Ok(());
            };
            visit(entity, &text)
        },
    )
}

fn validate_rebuild_definition(
    connection: &Connection,
    commit: HashId,
    index_name: &str,
    expected: &IndexDefinition,
) -> QueryResult<()> {
    let current = SchemaState::load(connection, commit)?;
    if current.indexes.get(index_name) == Some(expected) {
        return Ok(());
    }
    Err(QueryError::new(
        QueryErrorKind::Storage,
        "Semantic Index definition changed while rebuild was running",
    ))
}

fn finish_rebuild_stage(
    connection: &Connection,
    result: QueryResult<ManagedRebuildOutcome>,
) -> QueryResult<ManagedRebuildOutcome> {
    let cleanup =
        connection.execute_batch("DROP TABLE IF EXISTS temp._lithograph_semantic_rebuild_stage");
    match result {
        Ok(outcome) => {
            cleanup?;
            Ok(outcome)
        }
        Err(error) => {
            let _ = cleanup;
            Err(error)
        }
    }
}

fn resolve_runtime<'connection>(
    connection: &'connection Connection,
    index: &IndexDefinition,
) -> QueryResult<ManagedRuntime<'connection>> {
    storage::require_embedding_cache_format(connection)?;
    let Some((provider_name, provider_config, dimensions, similarity)) = semantic_definition(index)
    else {
        return Err(QueryError::internal(
            "Semantic Index is missing its versioned provider configuration",
        ));
    };
    let dimensions = usize::try_from(dimensions)
        .map_err(|_| QueryError::internal("Semantic Index dimension is too large"))?;
    let provider = RegisteredEmbeddingProvider::lookup(connection, provider_name)
        .map_err(map_provider_error)?;
    let config_json = serde_json::to_vec(provider_config).map_err(|error| {
        QueryError::internal(format!("failed to encode providerConfig: {error}"))
    })?;
    provider
        .validate(&config_json, dimensions)
        .map_err(map_provider_error)?;
    let space_hash = storage::embedding_cache_space_hash(
        provider_name,
        provider_config,
        dimensions as u64,
        provider.semantic_identity(),
    )?;
    Ok(ManagedRuntime {
        provider,
        config_json,
        dimensions,
        similarity: similarity.to_owned(),
        space_hash,
    })
}

fn resolve_embeddings(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    texts: &[String],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<BTreeMap<String, Vec<f32>>> {
    let unique = texts.iter().cloned().collect::<BTreeSet<_>>();
    let mut resolved = BTreeMap::new();
    let mut misses = Vec::new();
    for text in unique {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if let Some(vector) = storage::embedding_cache_lookup(
            connection,
            runtime.space_hash,
            &text,
            runtime.dimensions,
        )? {
            validate_runtime_embedding(runtime, &vector)?;
            resolved.insert(text, vector);
            continue;
        }
        if let Some(vector) = storage::embedding_query_cache_lookup(
            connection,
            runtime.space_hash,
            &text,
            runtime.dimensions,
        )? {
            validate_runtime_embedding(runtime, &vector)?;
            resolved.insert(text, vector);
            continue;
        }
        misses.push(text);
    }
    if misses.is_empty() {
        return Ok(resolved);
    }
    let refs = misses.iter().map(String::as_str).collect::<Vec<_>>();
    let values = runtime
        .provider
        .embed_batch(
            &runtime.config_json,
            &refs,
            runtime.dimensions,
            is_interrupted,
        )
        .map_err(map_provider_error)?;
    for (text, vector) in misses
        .into_iter()
        .zip(values.chunks_exact(runtime.dimensions))
    {
        let vector = vector.to_vec();
        validate_runtime_embedding(runtime, &vector)?;
        storage::embedding_query_cache_put(connection, runtime.space_hash, &text, &vector)?;
        resolved.insert(text, vector);
    }
    Ok(resolved)
}

fn validate_runtime_embedding(runtime: &ManagedRuntime<'_>, vector: &[f32]) -> QueryResult<()> {
    if runtime.similarity == "cosine" && !has_finite_nonzero_norm(vector) {
        return Err(QueryError::semantic(
            "Managed Semantic cosine similarity requires finite non-zero embeddings",
        ));
    }
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the bounded semantic source-batch flush keeps query state explicit"
)]
fn flush_query_source_batch(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    query_vector: &[f32],
    pending: &mut Vec<(SemanticEntity, String)>,
    pending_texts: &mut BTreeSet<String>,
    hits: &mut Vec<SemanticHit>,
    hnsw_entries: &mut ManagedHnswEntries,
    needed: usize,
    collect_hnsw: bool,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    if pending.is_empty() {
        return Ok(());
    }
    let texts = pending_texts.iter().cloned().collect::<Vec<_>>();
    let vectors = resolve_embeddings(connection, runtime, &texts, is_interrupted)?;
    for (entity, text) in pending.drain(..) {
        let vector = vectors.get(&text).ok_or_else(|| {
            QueryError::internal("semantic source embedding is missing after batch resolution")
        })?;
        hits.push(SemanticHit {
            entity,
            score: vector_similarity_numbers(vector, query_vector, &runtime.similarity)?,
        });
        if collect_hnsw {
            hnsw_entries.push((entity_id(entity), vector.clone()));
        }
    }
    pending_texts.clear();
    let trim_threshold = needed.saturating_mul(2).max(4_096);
    if hits.len() > trim_threshold {
        sort_semantic_hits(hits);
        hits.truncate(needed);
    }
    Ok(())
}

fn validate_target(index: &IndexDefinition, relationship_query: bool) -> QueryResult<()> {
    let relationship_index = matches!(index.target, IndexTarget::RelationshipProperties { .. });
    if relationship_index != relationship_query {
        return Err(QueryError::semantic(if relationship_query {
            "Semantic Relationship query requires a Relationship Semantic Index"
        } else {
            "Semantic Node query requires a Node Semantic Index"
        }));
    }
    Ok(())
}

fn source_property(index: &IndexDefinition) -> QueryResult<&str> {
    let properties = match &index.target {
        IndexTarget::NodeProperties { properties, .. }
        | IndexTarget::RelationshipProperties { properties, .. } => properties,
        IndexTarget::NodeLookup | IndexTarget::RelationshipLookup => {
            return Err(QueryError::internal("Semantic Index has a lookup target"));
        }
    };
    properties
        .first()
        .map(String::as_str)
        .ok_or_else(|| QueryError::internal("Semantic Index is missing its source Property"))
}

fn create_rebuild_stage(connection: &Connection) -> QueryResult<()> {
    connection.execute_batch(
        "DROP TABLE IF EXISTS temp._lithograph_semantic_rebuild_stage;         CREATE TEMP TABLE _lithograph_semantic_rebuild_stage(             text_hash BLOB NOT NULL CHECK(length(text_hash)=32),             text_value TEXT NOT NULL,             vector_blob BLOB NULL,             PRIMARY KEY(text_hash)         ) WITHOUT ROWID;",
    )?;
    Ok(())
}

fn stage_rebuild_text(connection: &Connection, text: &str) -> QueryResult<()> {
    let hash = storage::embedding_text_hash(text);
    let existing = connection
        .query_row(
            "SELECT text_value FROM temp._lithograph_semantic_rebuild_stage WHERE text_hash=?1",
            [hash.as_bytes().as_slice()],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing) = existing {
        if existing != text {
            return Err(QueryError::new(
                QueryErrorKind::Storage,
                "semantic rebuild detected a text hash collision",
            ));
        }
        return Ok(());
    }
    connection.execute(
        "INSERT INTO temp._lithograph_semantic_rebuild_stage(text_hash,text_value,vector_blob)          VALUES(?1,?2,NULL)",
        params![hash.as_bytes().as_slice(), text],
    )?;
    Ok(())
}

fn fill_rebuild_stage(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<(u64, u64)> {
    let mut cursor: Option<Vec<u8>> = None;
    let mut embedded_texts = 0_u64;
    let mut cache_hits = 0_u64;
    loop {
        let batch = read_rebuild_text_batch(connection, cursor.as_deref())?;
        if batch.is_empty() {
            break;
        }
        cursor = batch.last().map(|(hash, _)| hash.clone());
        let (misses, batch_hits) =
            resolve_rebuild_cache_hits(connection, runtime, &batch, is_interrupted)?;
        cache_hits = cache_hits.saturating_add(batch_hits);
        embedded_texts = embedded_texts.saturating_add(embed_rebuild_misses(
            connection,
            runtime,
            misses,
            is_interrupted,
        )?);
    }
    Ok((embedded_texts, cache_hits))
}

fn resolve_rebuild_cache_hits(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    batch: &[RebuildTextEntry],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RebuildCacheResolution> {
    let mut misses = Vec::new();
    let mut cache_hits = 0_u64;
    for (hash, text) in batch {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        let Some(vector) = storage::embedding_cache_lookup(
            connection,
            runtime.space_hash,
            text,
            runtime.dimensions,
        )?
        else {
            misses.push((hash.clone(), text.clone()));
            continue;
        };
        validate_runtime_embedding(runtime, &vector)?;
        stage_rebuild_vector(connection, hash, &vector)?;
        storage::embedding_query_cache_put(connection, runtime.space_hash, text, &vector)?;
        cache_hits = cache_hits.saturating_add(1);
    }
    Ok((misses, cache_hits))
}

fn embed_rebuild_misses(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    misses: Vec<RebuildTextEntry>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<u64> {
    if misses.is_empty() {
        return Ok(0);
    }
    let refs = misses
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>();
    let values = runtime
        .provider
        .embed_batch(
            &runtime.config_json,
            &refs,
            runtime.dimensions,
            is_interrupted,
        )
        .map_err(map_provider_error)?;
    let mut embedded = 0_u64;
    for ((hash, text), vector) in misses
        .into_iter()
        .zip(values.chunks_exact(runtime.dimensions))
    {
        validate_runtime_embedding(runtime, vector)?;
        stage_rebuild_vector(connection, &hash, vector)?;
        storage::embedding_query_cache_put(connection, runtime.space_hash, &text, vector)?;
        embedded = embedded.saturating_add(1);
    }
    Ok(embedded)
}

fn read_rebuild_text_batch(
    connection: &Connection,
    after: Option<&[u8]>,
) -> QueryResult<Vec<(Vec<u8>, String)>> {
    let mut statement = connection.prepare(
        "SELECT text_hash,text_value FROM temp._lithograph_semantic_rebuild_stage          WHERE vector_blob IS NULL AND (?1 IS NULL OR text_hash > ?1)          ORDER BY text_hash LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![after, i64::try_from(REBUILD_TEXT_BATCH).unwrap_or(i64::MAX)],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(QueryError::from)
}

fn stage_rebuild_vector(connection: &Connection, hash: &[u8], vector: &[f32]) -> QueryResult<()> {
    let blob = encode_stage_vector(vector);
    connection.execute(
        "UPDATE temp._lithograph_semantic_rebuild_stage SET vector_blob=?2 WHERE text_hash=?1",
        params![hash, blob],
    )?;
    Ok(())
}

fn publish_rebuild_stage(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    expected_index: &IndexDefinition,
    commit: HashId,
    index_name: &str,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    if connection.is_readonly("main")? {
        // A read-only database cannot publish the optional persistent cache,
        // but rebuild still has a complete TEMP embedding/HNSW result for this
        // connection. Revalidate the pinned definition before reporting
        // success just as the writable publish path does.
        return validate_rebuild_definition(connection, commit, index_name, expected_index);
    }
    connection.execute_batch("SAVEPOINT lithograph_semantic_rebuild_publish")?;
    let result = (|| {
        let current = SchemaState::load(connection, commit)?;
        if current.indexes.get(index_name) != Some(expected_index) {
            return Err(QueryError::new(
                QueryErrorKind::Storage,
                "Semantic Index definition changed before cache publish",
            ));
        }
        let mut cursor: Option<Vec<u8>> = None;
        loop {
            if is_interrupted() {
                return Err(QueryError::interrupted());
            }
            let batch =
                read_rebuild_publish_batch(connection, cursor.as_deref(), runtime.dimensions)?;
            if batch.is_empty() {
                break;
            }
            cursor = batch.last().map(|(hash, _)| hash.clone());
            let entries = batch
                .into_iter()
                .map(|(_, entry)| entry)
                .collect::<Vec<_>>();
            storage::embedding_cache_publish(
                connection,
                runtime.space_hash,
                runtime.dimensions,
                &entries,
            )?;
        }
        Ok(())
    })();
    match result {
        Ok(()) => {
            connection.execute_batch("RELEASE lithograph_semantic_rebuild_publish")?;
            Ok(())
        }
        Err(error) => {
            let _ = connection.execute_batch(
                "ROLLBACK TO lithograph_semantic_rebuild_publish;                  RELEASE lithograph_semantic_rebuild_publish",
            );
            Err(error)
        }
    }
}

fn read_rebuild_publish_batch(
    connection: &Connection,
    after: Option<&[u8]>,
    dimensions: usize,
) -> QueryResult<Vec<(Vec<u8>, EmbeddingCacheEntry)>> {
    let mut statement = connection.prepare(
        "SELECT text_hash,text_value,vector_blob FROM temp._lithograph_semantic_rebuild_stage          WHERE vector_blob IS NOT NULL AND (?1 IS NULL OR text_hash > ?1)          ORDER BY text_hash LIMIT ?2",
    )?;
    let rows = statement.query_map(
        params![
            after,
            i64::try_from(REBUILD_PUBLISH_BATCH).unwrap_or(i64::MAX)
        ],
        |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        },
    )?;
    rows.map(|row| {
        let (hash, text, blob) = row?;
        let vector = decode_stage_vector(&blob, dimensions)?;
        Ok((hash, EmbeddingCacheEntry { text, vector }))
    })
    .collect()
}

fn encode_stage_vector(vector: &[f32]) -> Vec<u8> {
    storage::encode_embedding_vector(vector)
}

fn decode_stage_vector(blob: &[u8], dimensions: usize) -> QueryResult<Vec<f32>> {
    if blob.len() != dimensions.saturating_mul(std::mem::size_of::<f32>()) {
        return Err(QueryError::internal(
            "semantic rebuild TEMP vector has invalid dimensions",
        ));
    }
    let mut vector = Vec::with_capacity(dimensions);
    for chunk in blob.as_chunks::<4>().0 {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return Err(QueryError::internal(
                "semantic rebuild TEMP vector contains a non-finite coordinate",
            ));
        }
        vector.push(value);
    }
    Ok(vector)
}

pub(crate) fn map_provider_error(error: ProviderError) -> QueryError {
    let kind = match error.kind {
        ProviderErrorKind::InvalidConfig | ProviderErrorKind::Missing => {
            QueryErrorKind::InvalidArgument
        }
        ProviderErrorKind::Io => QueryErrorKind::Io,
        ProviderErrorKind::Resource => QueryErrorKind::Resource,
        ProviderErrorKind::Cancelled => QueryErrorKind::Interrupted,
        ProviderErrorKind::InvalidRegistration | ProviderErrorKind::Internal => {
            QueryErrorKind::Internal
        }
    };
    let mut mapped = QueryError::new(kind, error.message);
    if kind == QueryErrorKind::Interrupted {
        mapped.sqlite_code = Some(rusqlite::ffi::SQLITE_INTERRUPT);
    }
    mapped
}
