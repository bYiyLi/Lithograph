use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};

use lithograph_embedding_provider::{
    ProviderError, ProviderErrorKind, RegisteredEmbeddingProvider,
};
use rusqlite::{Connection, OptionalExtension as _, params};

use crate::cypher::Value;
use crate::storage::{
    self, HashId, IndexConfiguration, IndexDefinition, IndexTarget, SchemaState, Snapshot,
    StandardIndexKind,
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
const EXECUTION_WORK_ENCODING_VERSION: &[u8] = b"LITHOGRAPH_SEMANTIC_EXECUTION_WORK_V1";
const EXECUTION_WORK_TABLE: &str = "_lithograph_semantic_execution_work";
const REBUILD_STAGE_TABLE: &str = "_lithograph_semantic_rebuild_stage";
static NEXT_EXECUTION_WORK: AtomicU64 = AtomicU64::new(1);
static NEXT_REBUILD_STAGE: AtomicU64 = AtomicU64::new(1);
type ManagedHnswEntries = Vec<(i64, Vec<f32>)>;
type RebuildTextEntry = (Vec<u8>, String);

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
}

struct ManagedRuntime<'connection> {
    provider: RegisteredEmbeddingProvider<'connection>,
    config_json: Vec<u8>,
    dimensions: usize,
    similarity: String,
    execution_space: [u8; 32],
    materialization_identity: [u8; 32],
}

struct ExecutionEmbeddingWork {
    id: i64,
}

struct RebuildStage {
    id: i64,
}

impl RebuildStage {
    fn create(connection: &Connection) -> QueryResult<Self> {
        initialize_execution_work(connection)?;
        let id = NEXT_REBUILD_STAGE.fetch_add(1, Ordering::Relaxed);
        let id = i64::try_from(id)
            .map_err(|_| QueryError::internal("semantic rebuild stage id overflow"))?;
        Ok(Self { id })
    }

    fn cleanup(&self, connection: &Connection) -> QueryResult<()> {
        connection.execute(
            &format!("DELETE FROM temp.{REBUILD_STAGE_TABLE} WHERE rebuild_id=?1"),
            [self.id],
        )?;
        Ok(())
    }
}

impl ExecutionEmbeddingWork {
    fn create(connection: &Connection) -> QueryResult<Self> {
        initialize_execution_work(connection)?;
        let id = NEXT_EXECUTION_WORK.fetch_add(1, Ordering::Relaxed);
        let id = i64::try_from(id)
            .map_err(|_| QueryError::internal("semantic execution work id overflow"))?;
        Ok(Self { id })
    }

    fn lookup(
        &self,
        connection: &Connection,
        execution_space: &[u8; 32],
        text: &str,
        dimensions: usize,
    ) -> QueryResult<Option<Vec<f32>>> {
        let row = connection
            .query_row(
                &format!(
                    "SELECT dimension,vector_blob FROM temp.{EXECUTION_WORK_TABLE} \
                     WHERE execution_id=?1 AND execution_space=?2 AND text_value=?3"
                ),
                params![self.id, execution_space.as_slice(), text],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        let Some((dimension, blob)) = row else {
            return Ok(None);
        };
        if dimension != i64::try_from(dimensions).unwrap_or(i64::MAX)
            || blob.len() != dimensions.saturating_mul(4)
        {
            return Err(QueryError::internal(
                "semantic execution work materialization is corrupt",
            ));
        }
        let mut vector = Vec::with_capacity(dimensions);
        for chunk in blob.as_chunks::<4>().0 {
            let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
            if !value.is_finite() {
                return Err(QueryError::internal(
                    "semantic execution work contains a non-finite vector",
                ));
            }
            vector.push(value);
        }
        Ok(Some(vector))
    }

    fn insert(
        &self,
        connection: &Connection,
        execution_space: &[u8; 32],
        text: &str,
        vector: &[f32],
    ) -> QueryResult<()> {
        let dimension = i64::try_from(vector.len())
            .map_err(|_| QueryError::internal("embedding dimension is too large"))?;
        let blob = encode_stage_vector(vector);
        connection.execute(
            &format!(
                "INSERT OR REPLACE INTO temp.{EXECUTION_WORK_TABLE}(\
                 execution_id,execution_space,text_value,dimension,vector_blob) \
                 VALUES(?1,?2,?3,?4,?5)"
            ),
            params![self.id, execution_space.as_slice(), text, dimension, blob],
        )?;
        Ok(())
    }

    fn drop_table(&self, connection: &Connection) -> QueryResult<()> {
        connection.execute(
            &format!("DELETE FROM temp.{EXECUTION_WORK_TABLE} WHERE execution_id=?1"),
            [self.id],
        )?;
        Ok(())
    }
}

pub(crate) fn initialize_execution_work(connection: &Connection) -> QueryResult<()> {
    let existing: i64 = connection.query_row(
        "SELECT count(*) FROM temp.sqlite_schema WHERE type='table' AND name IN (?1,?2)",
        params![EXECUTION_WORK_TABLE, REBUILD_STAGE_TABLE],
        |row| row.get(0),
    )?;
    if existing == 2 {
        return Ok(());
    }
    connection.execute_batch(&format!(
        "CREATE TEMP TABLE IF NOT EXISTS {EXECUTION_WORK_TABLE}(\
         execution_id INTEGER NOT NULL,\
         execution_space BLOB NOT NULL CHECK(length(execution_space)=32),\
         text_value TEXT NOT NULL,\
         dimension INTEGER NOT NULL CHECK(dimension BETWEEN 1 AND 4096),\
         vector_blob BLOB NOT NULL,\
         PRIMARY KEY(execution_id,execution_space,text_value)\
         ) WITHOUT ROWID;\
         CREATE TEMP TABLE IF NOT EXISTS {REBUILD_STAGE_TABLE}(\
         rebuild_id INTEGER NOT NULL,\
         text_hash BLOB NOT NULL CHECK(length(text_hash)=32),\
         text_value TEXT NOT NULL,\
         vector_blob BLOB NULL,\
         PRIMARY KEY(rebuild_id,text_hash)\
         ) WITHOUT ROWID"
    ))?;
    Ok(())
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
    let work = ExecutionEmbeddingWork::create(connection)?;
    let result = query_with_work(
        connection,
        snapshot,
        graph_view,
        index,
        input,
        is_interrupted,
        &work,
    );
    let cleanup = work.drop_table(connection);
    match result {
        Ok(hits) => {
            cleanup?;
            Ok(hits)
        }
        Err(error) => {
            let _ = cleanup;
            Err(error)
        }
    }
}

fn query_with_work(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &ManagedQueryInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
    work: &ExecutionEmbeddingWork,
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
    let query_vector =
        resolve_query_embedding(connection, &runtime, work, input.query, is_interrupted)?;
    let cache_key = managed_hnsw_cache_key(snapshot, index, runtime.materialization_identity)?;
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
        work,
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
    materialization_identity: [u8; 32],
) -> QueryResult<String> {
    let base = semantic_cache_digest(snapshot, index, b"LITHOGRAPH_MANAGED_SEMANTIC_HNSW_V1")?;
    Ok(format!("{base}:{}", hex_bytes(&materialization_identity)))
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
    work: &ExecutionEmbeddingWork,
    query: &str,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<f32>> {
    resolve_embeddings(
        connection,
        runtime,
        work,
        &[query.to_owned()],
        is_interrupted,
    )?
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
    work: &ExecutionEmbeddingWork,
    query_vector: &[f32],
    needed: usize,
    collect_hnsw: bool,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<(Vec<SemanticHit>, ManagedHnswEntries)> {
    let property = source_property(index)?.to_owned();
    let mut state = QueryHitAccumulator::new(needed, collect_hnsw);
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
            state.pending_texts.insert(text.clone());
            state.pending.push((entity, text));
            if state.pending.len() >= QUERY_SOURCE_BATCH_ENTITIES
                || state.pending_texts.len() >= QUERY_SOURCE_BATCH_TEXTS
            {
                state.flush(connection, runtime, work, query_vector, is_interrupted)?;
            }
            Ok(())
        },
    )?;
    state.flush(connection, runtime, work, query_vector, is_interrupted)?;
    Ok((state.hits, state.hnsw_entries))
}

pub(crate) fn rebuild(
    connection: &Connection,
    index_name: &str,
    version: &str,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<ManagedRebuildOutcome> {
    let (commit, index) = resolve_rebuild_target(connection, index_name, version)?;
    let runtime = resolve_runtime(connection, &index)?;
    let snapshot = Snapshot::resolve(connection, commit)?;

    let stage = RebuildStage::create(connection)?;
    let result = rebuild_staged(
        connection,
        index_name,
        commit,
        &index,
        &snapshot,
        &runtime,
        &stage,
        is_interrupted,
    );
    finish_rebuild_stage(connection, &stage, result)
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
    stage: &RebuildStage,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<ManagedRebuildOutcome> {
    let indexed_entities =
        stage_rebuild_sources(connection, snapshot, index, stage, is_interrupted)?;
    let embedded_texts = fill_rebuild_stage(connection, runtime, stage, is_interrupted)?;
    validate_rebuild_definition(connection, commit, index_name, index)?;
    let cache_key = managed_hnsw_cache_key(snapshot, index, runtime.materialization_identity)?;
    let hnsw_entries = collect_rebuild_hnsw_entries(
        connection,
        snapshot,
        index,
        runtime.dimensions,
        stage,
        is_interrupted,
    )?;
    let built_hnsw = build_managed_hnsw_cache(
        connection,
        &cache_key,
        &runtime.similarity,
        hnsw_entries,
        is_interrupted,
    )?;
    if let Err(error) = validate_rebuild_definition(connection, commit, index_name, index) {
        if built_hnsw {
            let _ = invalidate_managed_hnsw_cache(connection, &cache_key);
        }
        return Err(error);
    }
    Ok(ManagedRebuildOutcome {
        commit,
        indexed_entities,
        embedded_texts,
    })
}

fn collect_rebuild_hnsw_entries(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    dimensions: usize,
    stage: &RebuildStage,
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
                rebuild_stage_vector(connection, stage, text, dimensions)?,
            ));
            Ok(())
        },
    )?;
    Ok(entries)
}

fn rebuild_stage_vector(
    connection: &Connection,
    stage: &RebuildStage,
    text: &str,
    dimensions: usize,
) -> QueryResult<Vec<f32>> {
    let hash = *blake3::hash(text.as_bytes()).as_bytes();
    let (stored_text, blob) = connection
        .query_row(
            &format!(
                "SELECT text_value,vector_blob FROM temp.{REBUILD_STAGE_TABLE} \
                 WHERE rebuild_id=?1 AND text_hash=?2"
            ),
            params![stage.id, hash.as_slice()],
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
    stage: &RebuildStage,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<u64> {
    let mut indexed_entities = 0_u64;
    visit_rebuild_sources(connection, snapshot, index, is_interrupted, |_, text| {
        indexed_entities = indexed_entities.saturating_add(1);
        stage_rebuild_text(connection, stage, text)
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
    stage: &RebuildStage,
    result: QueryResult<ManagedRebuildOutcome>,
) -> QueryResult<ManagedRebuildOutcome> {
    let cleanup = stage.cleanup(connection);
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
    let mut execution_hasher = blake3::Hasher::new();
    execution_hasher.update(EXECUTION_WORK_ENCODING_VERSION);
    execution_hasher.update(&(provider_name.len() as u64).to_le_bytes());
    execution_hasher.update(provider_name.as_bytes());
    execution_hasher.update(&(config_json.len() as u64).to_le_bytes());
    execution_hasher.update(&config_json);
    execution_hasher.update(&(dimensions as u64).to_le_bytes());
    execution_hasher.update(b"FLOAT32");
    execution_hasher.update(&(provider.semantic_identity().len() as u64).to_le_bytes());
    execution_hasher.update(provider.semantic_identity().as_bytes());
    let execution_space = *execution_hasher.finalize().as_bytes();

    let mut materialization_hasher = blake3::Hasher::new();
    materialization_hasher.update(b"LITHOGRAPH_MANAGED_SEMANTIC_RUNTIME_V1");
    materialization_hasher.update(provider.semantic_identity().as_bytes());
    let materialization_identity = *materialization_hasher.finalize().as_bytes();
    Ok(ManagedRuntime {
        provider,
        config_json,
        dimensions,
        similarity: similarity.to_owned(),
        execution_space,
        materialization_identity,
    })
}

fn resolve_embeddings(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    work: &ExecutionEmbeddingWork,
    texts: &[String],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<BTreeMap<String, Vec<f32>>> {
    let mut resolved = BTreeMap::new();
    let mut misses = Vec::new();
    for text in texts.iter().cloned().collect::<BTreeSet<_>>() {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if let Some(vector) = work.lookup(
            connection,
            &runtime.execution_space,
            &text,
            runtime.dimensions,
        )? {
            validate_runtime_embedding(runtime, &vector)?;
            resolved.insert(text, vector);
        } else {
            misses.push(text);
        }
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
        validate_runtime_embedding(runtime, vector)?;
        work.insert(connection, &runtime.execution_space, &text, vector)?;
        resolved.insert(text, vector.to_vec());
    }
    Ok(resolved)
}

fn hex_bytes(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        out.push(char::from(HEX[usize::from(byte >> 4)]));
        out.push(char::from(HEX[usize::from(byte & 0x0f)]));
    }
    out
}

fn validate_runtime_embedding(runtime: &ManagedRuntime<'_>, vector: &[f32]) -> QueryResult<()> {
    if runtime.similarity == "cosine" && !has_finite_nonzero_norm(vector) {
        return Err(QueryError::semantic(
            "Managed Semantic cosine similarity requires finite non-zero embeddings",
        ));
    }
    Ok(())
}

struct QueryHitAccumulator {
    pending: Vec<(SemanticEntity, String)>,
    pending_texts: BTreeSet<String>,
    hits: Vec<SemanticHit>,
    hnsw_entries: ManagedHnswEntries,
    needed: usize,
    collect_hnsw: bool,
}

impl QueryHitAccumulator {
    fn new(needed: usize, collect_hnsw: bool) -> Self {
        Self {
            pending: Vec::new(),
            pending_texts: BTreeSet::new(),
            hits: Vec::new(),
            hnsw_entries: ManagedHnswEntries::new(),
            needed,
            collect_hnsw,
        }
    }

    fn flush(
        &mut self,
        connection: &Connection,
        runtime: &ManagedRuntime<'_>,
        work: &ExecutionEmbeddingWork,
        query_vector: &[f32],
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let texts = self.pending_texts.iter().cloned().collect::<Vec<_>>();
        let vectors = resolve_embeddings(connection, runtime, work, &texts, is_interrupted)?;
        for (entity, text) in self.pending.drain(..) {
            let vector = vectors.get(&text).ok_or_else(|| {
                QueryError::internal("semantic source embedding is missing after batch resolution")
            })?;
            self.hits.push(SemanticHit {
                entity,
                score: vector_similarity_numbers(vector, query_vector, &runtime.similarity)?,
            });
            if self.collect_hnsw {
                self.hnsw_entries.push((entity_id(entity), vector.clone()));
            }
        }
        self.pending_texts.clear();
        let trim_threshold = self.needed.saturating_mul(2).max(4_096);
        if self.hits.len() > trim_threshold {
            sort_semantic_hits(&mut self.hits);
            self.hits.truncate(self.needed);
        }
        Ok(())
    }
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

fn stage_rebuild_text(
    connection: &Connection,
    stage: &RebuildStage,
    text: &str,
) -> QueryResult<()> {
    let hash = *blake3::hash(text.as_bytes()).as_bytes();
    let existing = connection
        .query_row(
            &format!(
                "SELECT text_value FROM temp.{REBUILD_STAGE_TABLE} \
                 WHERE rebuild_id=?1 AND text_hash=?2"
            ),
            params![stage.id, hash.as_slice()],
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
        &format!(
            "INSERT INTO temp.{REBUILD_STAGE_TABLE}(rebuild_id,text_hash,text_value,vector_blob) \
             VALUES(?1,?2,?3,NULL)"
        ),
        params![stage.id, hash.as_slice(), text],
    )?;
    Ok(())
}

fn fill_rebuild_stage(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    stage: &RebuildStage,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<u64> {
    let mut cursor: Option<Vec<u8>> = None;
    let mut embedded_texts = 0_u64;
    loop {
        let batch = read_rebuild_text_batch(connection, stage, cursor.as_deref())?;
        if batch.is_empty() {
            break;
        }
        cursor = batch.last().map(|(hash, _)| hash.clone());
        embedded_texts = embedded_texts.saturating_add(embed_rebuild_misses(
            connection,
            runtime,
            stage,
            batch,
            is_interrupted,
        )?);
    }
    Ok(embedded_texts)
}

fn embed_rebuild_misses(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    stage: &RebuildStage,
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
    for ((hash, _text), vector) in misses
        .into_iter()
        .zip(values.chunks_exact(runtime.dimensions))
    {
        validate_runtime_embedding(runtime, vector)?;
        stage_rebuild_vector(connection, stage, &hash, vector)?;
        embedded = embedded.saturating_add(1);
    }
    Ok(embedded)
}

fn read_rebuild_text_batch(
    connection: &Connection,
    stage: &RebuildStage,
    after: Option<&[u8]>,
) -> QueryResult<Vec<(Vec<u8>, String)>> {
    let mut statement = connection.prepare(&format!(
        "SELECT text_hash,text_value FROM temp.{REBUILD_STAGE_TABLE} \
             WHERE rebuild_id=?1 AND vector_blob IS NULL AND (?2 IS NULL OR text_hash > ?2) \
             ORDER BY text_hash LIMIT ?3"
    ))?;
    let rows = statement.query_map(
        params![
            stage.id,
            after,
            i64::try_from(REBUILD_TEXT_BATCH).unwrap_or(i64::MAX)
        ],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(QueryError::from)
}

fn stage_rebuild_vector(
    connection: &Connection,
    stage: &RebuildStage,
    hash: &[u8],
    vector: &[f32],
) -> QueryResult<()> {
    let blob = encode_stage_vector(vector);
    connection.execute(
        &format!(
            "UPDATE temp.{REBUILD_STAGE_TABLE} SET vector_blob=?3 \
             WHERE rebuild_id=?1 AND text_hash=?2"
        ),
        params![stage.id, hash, blob],
    )?;
    Ok(())
}

fn encode_stage_vector(vector: &[f32]) -> Vec<u8> {
    let mut blob = Vec::with_capacity(vector.len().saturating_mul(4));
    for value in vector {
        blob.extend_from_slice(&value.to_le_bytes());
    }
    blob
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
