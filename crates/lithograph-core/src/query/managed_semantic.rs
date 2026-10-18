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

use super::expression::clone_string_controlled;
use super::graph::ResolvedGraphView;
use super::resources::{
    ExecutionControl, MemoryReservation, QueryResources, allocation_bytes, checked_add,
    checked_mul, json_bytes_controlled, map_entry_bytes,
};
mod resources;
use super::semantic_index::{
    SemanticEntity, SemanticHit, SemanticMembership, build_managed_hnsw_cache, entity_id,
    has_finite_nonzero_norm_controlled, invalidate_managed_hnsw_cache, query_managed_hnsw_cache,
    reserve_managed_hnsw_workspace, semantic_cache_digest, semantic_entity_matches,
    semantic_entity_visible, semantic_membership, semantic_property_controlled,
    sort_semantic_hits_controlled, vector_similarity_numbers_controlled, visit_indexed_entities,
};
use super::{QueryError, QueryErrorKind, QueryResult};
use resources::{
    Budgeted, Buffer, copy_vector, encode_vector, load_schema, push_text, read_vector, vector_bytes,
};

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
    _memory: MemoryReservation,
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
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<Budgeted<Vec<f32>>>> {
        read_vector(
            connection,
            &format!(
                "SELECT dimension,vector_blob FROM temp.{EXECUTION_WORK_TABLE} \
                      WHERE execution_id=?1 AND execution_space=?2 AND text_value=?3"
            ),
            params![self.id, execution_space.as_slice(), text],
            dimensions,
            control,
        )
    }

    fn insert(
        &self,
        connection: &Connection,
        execution_space: &[u8; 32],
        text: &str,
        vector: &[f32],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let dimension = i64::try_from(vector.len())
            .map_err(|_| QueryError::internal("embedding dimension is too large"))?;
        let blob = encode_vector(vector, control)?;
        connection.execute(
            &format!(
                "INSERT OR REPLACE INTO temp.{EXECUTION_WORK_TABLE}(\
                 execution_id,execution_space,text_value,dimension,vector_blob) \
                 VALUES(?1,?2,?3,?4,?5)"
            ),
            params![
                self.id,
                execution_space.as_slice(),
                text,
                dimension,
                &blob.value
            ],
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

pub(crate) fn validate_transition_controlled(
    connection: &Connection,
    previous: &SchemaState,
    next: &SchemaState,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let check = || super::check_interrupted(is_interrupted).map_err(QueryError::into_storage_error);
    for (name, definition) in &next.indexes {
        super::check_interrupted(is_interrupted)?;
        if definition.kind != StandardIndexKind::Semantic {
            continue;
        }
        if previous
            .indexes
            .get(name)
            .map(|previous| previous.canonical_eq_checked(definition, &check))
            .transpose()?
            .unwrap_or(false)
        {
            continue;
        }
        super::check_interrupted(is_interrupted)?;
        validate_definition(connection, definition)?;
        super::check_interrupted(is_interrupted)?;
    }
    Ok(())
}

pub(crate) fn query_controlled(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &ManagedQueryInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
    resources: &QueryResources,
) -> QueryResult<(Vec<SemanticHit>, MemoryReservation)> {
    let control = ExecutionControl::new(resources, is_interrupted);
    control.check()?;
    let _metadata = control.reserve(super::semantic_index::search_index_metadata_bytes(
        index, &control,
    )?)?;
    super::semantic_index::validate_query_index_access(index, graph_view)?;
    let work = ExecutionEmbeddingWork::create(connection)?;
    let result = query_with_work(
        connection, snapshot, graph_view, index, input, &control, &work,
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
    control: &ExecutionControl<'_>,
    work: &ExecutionEmbeddingWork,
) -> QueryResult<(Vec<SemanticHit>, MemoryReservation)> {
    validate_target(index, input.relationship_query)?;
    let runtime = resolve_runtime(connection, index, control)?;
    if input.limit == 0 {
        return Ok((Vec::new(), control.reserve(0)?));
    }
    let needed = input
        .skip
        .checked_add(input.limit)
        .ok_or_else(|| QueryError::invalid_argument("semantic skip + limit is too large"))?;
    let query_vector = resolve_query_embedding(connection, &runtime, work, input.query, control)?;
    let cache_key = managed_hnsw_cache_key(snapshot, index, runtime.materialization_identity)?;
    if let Some(result) = query_cached_hits(
        connection,
        snapshot,
        graph_view,
        (index, input),
        &runtime,
        (&cache_key, needed, &query_vector),
        control,
    )? {
        return Ok(result);
    }
    let collect_hnsw = graph_view.is_full_graph();
    let (mut hits, hnsw_entries) = collect_query_hits(
        connection,
        snapshot,
        graph_view,
        index,
        (&runtime, work, &query_vector.value, needed, collect_hnsw),
        control,
    )?;
    trim_hits(&mut hits.value, needed, input.skip, input.limit, control)?;
    if collect_hnsw {
        build_query_cache(connection, &cache_key, &runtime, hnsw_entries, control)?;
    }
    Ok((hits.value, hits.memory))
}

fn build_query_cache(
    connection: &Connection,
    cache_key: &str,
    runtime: &ManagedRuntime<'_>,
    entries: Budgeted<ManagedHnswEntries>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let _workspace =
        reserve_managed_hnsw_workspace(entries.value.len(), runtime.dimensions, control)?;
    build_managed_hnsw_cache(
        connection,
        cache_key,
        &runtime.similarity,
        entries.value,
        &|| control.check().is_err(),
        control.resources(),
    )?;
    Ok(())
}

fn query_cached_hits(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    source: (&IndexDefinition, &ManagedQueryInput<'_>),
    runtime: &ManagedRuntime<'_>,
    cache: (&str, usize, &Budgeted<Vec<f32>>),
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(Vec<SemanticHit>, MemoryReservation)>> {
    let (index, input) = source;
    let (cache_key, needed, query_vector) = cache;
    let membership = semantic_membership(connection, index)?;
    let property = source_property(index)?;
    if let Some(mut cached) = query_managed_hnsw_cache(
        connection,
        (cache_key, control.resources()),
        &query_vector.value,
        &runtime.similarity,
        needed,
        &|| control.check().is_err(),
        |owner_id| {
            resolve_cached_entity(
                snapshot,
                graph_view,
                index,
                &membership,
                property,
                owner_id,
                control,
            )
        },
    )? {
        let result_memory = cached.memory.take().ok_or_else(|| {
            QueryError::internal("managed cache result is missing its memory reservation")
        })?;
        trim_hits(&mut cached.hits, needed, input.skip, input.limit, control)?;
        return Ok(Some((cached.hits, result_memory)));
    }
    Ok(None)
}

fn trim_hits(
    hits: &mut Vec<SemanticHit>,
    needed: usize,
    skip: usize,
    limit: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let _sorting = control.reserve(allocation_bytes::<SemanticHit>(hits.len())?)?;
    control.check()?;
    sort_semantic_hits_controlled(hits, Some(control))?;
    control.check()?;
    hits.truncate(needed);
    hits.drain(..skip.min(hits.len()));
    hits.truncate(limit);
    Ok(())
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
    control: &ExecutionControl<'_>,
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
        || !semantic_entity_visible(snapshot, graph_view, entity, Some(control))?
        || !matches!(
            semantic_property_controlled(snapshot, entity, property, Some(control))?.0,
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
    control: &ExecutionControl<'_>,
) -> QueryResult<Budgeted<Vec<f32>>> {
    let mut texts = Buffer::new(control.resources())?;
    push_text(&mut texts, query, control)?;
    let mut resolved = resolve_embeddings(connection, runtime, work, &texts.values, control)?;
    let value = resolved
        .value
        .remove(query)
        .ok_or_else(|| QueryError::internal("semantic query embedding is missing"))?;
    Ok(Budgeted {
        value,
        memory: resolved.memory,
    })
}

fn collect_query_hits(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: (
        &ManagedRuntime<'_>,
        &ExecutionEmbeddingWork,
        &[f32],
        usize,
        bool,
    ),
    control: &ExecutionControl<'_>,
) -> QueryResult<(Budgeted<Vec<SemanticHit>>, Budgeted<ManagedHnswEntries>)> {
    let (runtime, work, query_vector, needed, collect_hnsw) = input;
    let property = source_property(index)?;
    let mut state = QueryHitAccumulator::new(needed, collect_hnsw, control.resources())?;
    let is_interrupted = || control.check().is_err();
    visit_indexed_entities(
        connection,
        snapshot,
        index,
        Some(graph_view),
        &is_interrupted,
        Some(control),
        |entity| {
            let (value, _source_memory) =
                semantic_property_controlled(snapshot, entity, property, Some(control))?;
            let Value::String(text) = value else {
                return Ok(());
            };
            state.push(entity, text, control)?;
            if state.pending.values.len() >= QUERY_SOURCE_BATCH_ENTITIES
                || state.pending_texts.len() >= QUERY_SOURCE_BATCH_TEXTS
            {
                state.flush(connection, runtime, work, query_vector, control)?;
            }
            Ok(())
        },
    )?;
    state.flush(connection, runtime, work, query_vector, control)?;
    let (value, memory) = state.hits.into_parts();
    let hits = Budgeted { value, memory };
    let (value, memory) = state.hnsw_entries.into_parts();
    Ok((hits, Budgeted { value, memory }))
}

pub(crate) fn rebuild_controlled(
    connection: &Connection,
    index_name: &str,
    version: &str,
    is_interrupted: &dyn Fn() -> bool,
    resources: &QueryResources,
) -> QueryResult<ManagedRebuildOutcome> {
    let control = ExecutionControl::new(resources, is_interrupted);
    control.check()?;
    let (commit, index, _index_memory) =
        resolve_rebuild_target(connection, index_name, version, &control)?;
    let runtime = resolve_runtime(connection, &index, &control)?;
    let snapshot = super::graph::resolve_snapshot_with_control(connection, commit, &control)?;

    let stage = RebuildStage::create(connection)?;
    let result = rebuild_staged(
        connection,
        (index_name, commit),
        &index,
        &snapshot,
        &runtime,
        &stage,
        &control,
    );
    finish_rebuild_stage(connection, &stage, result)
}

fn resolve_rebuild_target(
    connection: &Connection,
    index_name: &str,
    version: &str,
    control: &ExecutionControl<'_>,
) -> QueryResult<(HashId, IndexDefinition, MemoryReservation)> {
    let commit = storage::resolve_version_descriptor(connection, version)?;
    let (schema, _schema_memory) = load_schema(connection, commit, control)?;
    let index = schema.indexes.get(index_name).ok_or_else(|| {
        QueryError::invalid_argument(format!(
            "Semantic Index {index_name:?} was not found at {version}"
        ))
    })?;
    if index.kind != StandardIndexKind::Semantic {
        return Err(QueryError::invalid_argument(format!(
            "Index {index_name:?} is not a Semantic Index"
        )));
    }
    let memory = control.reserve(super::semantic_index::search_index_metadata_bytes(
        index, control,
    )?)?;
    Ok((commit, index.clone(), memory))
}

fn rebuild_staged(
    connection: &Connection,
    target: (&str, HashId),
    index: &IndexDefinition,
    snapshot: &Snapshot<'_>,
    runtime: &ManagedRuntime<'_>,
    stage: &RebuildStage,
    control: &ExecutionControl<'_>,
) -> QueryResult<ManagedRebuildOutcome> {
    let (index_name, commit) = target;
    let indexed_entities = stage_rebuild_sources(connection, snapshot, index, stage, control)?;
    let embedded_texts = fill_rebuild_stage(connection, runtime, stage, control)?;
    validate_rebuild_definition(connection, commit, index_name, index, control)?;
    let cache_key = managed_hnsw_cache_key(snapshot, index, runtime.materialization_identity)?;
    let hnsw_entries = collect_rebuild_hnsw_entries(
        connection,
        snapshot,
        index,
        runtime.dimensions,
        stage,
        control,
    )?;
    let _workspace =
        reserve_managed_hnsw_workspace(hnsw_entries.value.len(), runtime.dimensions, control)?;
    let built_hnsw = build_managed_hnsw_cache(
        connection,
        &cache_key,
        &runtime.similarity,
        hnsw_entries.value,
        &|| control.check().is_err(),
        control.resources(),
    )?;
    if let Err(error) = validate_rebuild_definition(connection, commit, index_name, index, control)
    {
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
    control: &ExecutionControl<'_>,
) -> QueryResult<Budgeted<ManagedHnswEntries>> {
    let mut entries = Buffer::new(control.resources())?;
    visit_rebuild_sources(connection, snapshot, index, control, |entity, text| {
        let vector = rebuild_stage_vector(connection, stage, text, dimensions, control)?;
        entries.absorb(vector.memory)?;
        entries.push((entity_id(entity), vector.value), 0)?;
        Ok(())
    })?;
    let (value, memory) = entries.into_parts();
    Ok(Budgeted { value, memory })
}

fn rebuild_stage_vector(
    connection: &Connection,
    stage: &RebuildStage,
    text: &str,
    dimensions: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<Budgeted<Vec<f32>>> {
    control.bytes(text.len() as u64)?;
    let hash = *blake3::hash(text.as_bytes()).as_bytes();
    read_vector(
        connection,
        &format!(
            "SELECT length(vector_blob)/4,vector_blob FROM temp.{REBUILD_STAGE_TABLE} \
                  WHERE rebuild_id=?1 AND text_hash=?2 AND text_value=?3"
        ),
        params![stage.id, hash.as_slice(), text],
        dimensions,
        control,
    )?
    .ok_or_else(|| QueryError::internal("semantic rebuild TEMP vector is missing"))
}

fn stage_rebuild_sources(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    stage: &RebuildStage,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let mut indexed_entities = 0_u64;
    visit_rebuild_sources(connection, snapshot, index, control, |_, text| {
        indexed_entities = indexed_entities.saturating_add(1);
        stage_rebuild_text(connection, stage, text, control)
    })?;
    Ok(indexed_entities)
}

fn visit_rebuild_sources(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
    mut visit: impl FnMut(SemanticEntity, &str) -> QueryResult<()>,
) -> QueryResult<()> {
    let property = source_property(index)?;
    visit_indexed_entities(
        connection,
        snapshot,
        index,
        None,
        &|| control.check().is_err(),
        Some(control),
        |entity| {
            control.check()?;
            let (value, _memory) =
                semantic_property_controlled(snapshot, entity, property, Some(control))?;
            let Value::String(text) = value else {
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
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let (current, _memory) = load_schema(connection, commit, control)?;
    if current
        .indexes
        .get(index_name)
        .map(|current| current.canonical_eq(expected))
        .transpose()?
        .unwrap_or(false)
    {
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
    control: &ExecutionControl<'_>,
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
    let config_bytes = checked_mul(json_bytes_controlled(provider_config, control)?, 12)?;
    let mut runtime_memory =
        control.reserve(checked_add(config_bytes, similarity.len() as u64)?)?;
    let config_json = serde_json::to_vec(provider_config).map_err(|error| {
        QueryError::internal(format!("failed to encode providerConfig: {error}"))
    })?;
    runtime_memory.resize(checked_add(
        config_json.capacity() as u64,
        similarity.len() as u64,
    )?)?;
    control.check()?;
    provider
        .validate(&config_json, dimensions)
        .map_err(map_provider_error)?;
    control.check()?;
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
        _memory: runtime_memory,
    })
}

fn resolve_embeddings(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    work: &ExecutionEmbeddingWork,
    texts: &[String],
    control: &ExecutionControl<'_>,
) -> QueryResult<Budgeted<BTreeMap<String, Vec<f32>>>> {
    let mut resolved = BTreeMap::new();
    let mut memory = control.reserve(0)?;
    let mut misses = Buffer::new(control.resources())?;
    for text in texts {
        control.check()?;
        memory.grow(checked_add(
            map_entry_bytes::<Vec<f32>>()?,
            text.len() as u64,
        )?)?;
        if let Some(vector) = work.lookup(
            connection,
            &runtime.execution_space,
            text,
            runtime.dimensions,
            control,
        )? {
            insert_cached_embedding(runtime, &mut resolved, &mut memory, text, vector, control)?;
        } else {
            push_text(&mut misses, text, control)?;
        }
    }
    if !misses.values.is_empty() {
        fill_embedding_misses(
            connection,
            runtime,
            work,
            &misses.values,
            &mut resolved,
            &mut memory,
            control,
        )?;
    }
    Ok(Budgeted {
        value: resolved,
        memory,
    })
}

fn insert_cached_embedding(
    runtime: &ManagedRuntime<'_>,
    resolved: &mut BTreeMap<String, Vec<f32>>,
    memory: &mut MemoryReservation,
    text: &str,
    vector: Budgeted<Vec<f32>>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    validate_runtime_embedding(runtime, &vector.value, control)?;
    memory.merge(vector.memory)?;
    resolved.insert(clone_string_controlled(text, Some(control))?, vector.value);
    Ok(())
}

fn fill_embedding_misses(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    work: &ExecutionEmbeddingWork,
    misses: &[String],
    resolved: &mut BTreeMap<String, Vec<f32>>,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let _refs_memory = control.reserve(allocation_bytes::<[&str; 2]>(misses.len())?)?;
    let refs = misses.iter().map(String::as_str).collect::<Vec<_>>();
    let bytes = checked_mul(vector_bytes(runtime.dimensions)?, misses.len() as u64)?;
    let _provider_output = control.reserve(bytes)?;
    control.check()?;
    let values = runtime
        .provider
        .embed_batch(&runtime.config_json, &refs, runtime.dimensions, &|| {
            control.check().is_err()
        })
        .map_err(map_provider_error)?;
    control.check()?;
    memory.grow(bytes)?;
    for (text, vector) in misses.iter().zip(values.chunks_exact(runtime.dimensions)) {
        insert_new_embedding(connection, runtime, work, text, vector, resolved, control)?;
    }
    Ok(())
}

fn insert_new_embedding(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    work: &ExecutionEmbeddingWork,
    text: &str,
    vector: &[f32],
    resolved: &mut BTreeMap<String, Vec<f32>>,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.check()?;
    validate_runtime_embedding(runtime, vector, control)?;
    work.insert(connection, &runtime.execution_space, text, vector, control)?;
    resolved.insert(
        clone_string_controlled(text, Some(control))?,
        copy_vector(vector, control)?,
    );
    Ok(())
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

fn validate_runtime_embedding(
    runtime: &ManagedRuntime<'_>,
    vector: &[f32],
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    if runtime.similarity == "cosine"
        && !has_finite_nonzero_norm_controlled(vector, &|| control.check().is_err())?
    {
        return Err(QueryError::semantic(
            "Managed Semantic cosine similarity requires finite non-zero embeddings",
        ));
    }
    Ok(())
}

struct QueryHitAccumulator {
    pending: Buffer<(SemanticEntity, String)>,
    pending_texts: BTreeSet<String>,
    pending_text_memory: MemoryReservation,
    hits: Buffer<SemanticHit>,
    hnsw_entries: Buffer<(i64, Vec<f32>)>,
    needed: usize,
    collect_hnsw: bool,
}

impl QueryHitAccumulator {
    fn new(needed: usize, collect_hnsw: bool, resources: &QueryResources) -> QueryResult<Self> {
        Ok(Self {
            pending: Buffer::new(resources)?,
            pending_texts: BTreeSet::new(),
            pending_text_memory: resources.reserve(0)?,
            hits: Buffer::new(resources)?,
            hnsw_entries: Buffer::new(resources)?,
            needed,
            collect_hnsw,
        })
    }

    fn push(
        &mut self,
        entity: SemanticEntity,
        text: String,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        control.check()?;
        if !self.pending_texts.contains(&text) {
            self.pending_text_memory
                .grow(checked_add(map_entry_bytes::<()>()?, text.len() as u64)?)?;
            self.pending_texts
                .insert(clone_string_controlled(&text, Some(control))?);
        }
        self.pending.reserve(text.capacity() as u64)?;
        self.pending.push((entity, text), 0)?;
        Ok(())
    }

    fn flush(
        &mut self,
        connection: &Connection,
        runtime: &ManagedRuntime<'_>,
        work: &ExecutionEmbeddingWork,
        query_vector: &[f32],
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.pending.values.is_empty() {
            return Ok(());
        }
        let mut texts = Buffer::new(control.resources())?;
        for text in &self.pending_texts {
            push_text(&mut texts, text, control)?;
        }
        let vectors = resolve_embeddings(connection, runtime, work, &texts.values, control)?;
        self.append_hits(runtime, query_vector, &vectors.value, control)?;
        self.pending.clear()?;
        self.pending_texts.clear();
        self.pending_text_memory.resize(0)?;
        if self.hits.values.len() > self.needed.saturating_mul(2).max(4096) {
            let _sorting =
                control.reserve(allocation_bytes::<SemanticHit>(self.hits.values.len())?)?;
            sort_semantic_hits_controlled(&mut self.hits.values, Some(control))?;
            self.hits.values.truncate(self.needed);
        }
        Ok(())
    }
    fn append_hits(
        &mut self,
        runtime: &ManagedRuntime<'_>,
        query_vector: &[f32],
        vectors: &BTreeMap<String, Vec<f32>>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for (entity, text) in &self.pending.values {
            control.check()?;
            let vector = vectors.get(text).ok_or_else(|| {
                QueryError::internal("semantic source embedding is missing after batch resolution")
            })?;
            self.hits.push(
                SemanticHit {
                    entity: *entity,
                    score: vector_similarity_numbers_controlled(
                        vector,
                        query_vector,
                        &runtime.similarity,
                        &|| control.check().is_err(),
                    )?,
                },
                0,
            )?;
            if self.collect_hnsw {
                let bytes = vector_bytes(vector.len())?;
                self.hnsw_entries.reserve(bytes)?;
                self.hnsw_entries
                    .push((entity_id(*entity), copy_vector(vector, control)?), 0)?;
            }
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
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.bytes(text.len() as u64)?;
    let hash = *blake3::hash(text.as_bytes()).as_bytes();
    let existing = connection
        .query_row(
            &format!(
                "SELECT text_value=?3 FROM temp.{REBUILD_STAGE_TABLE} \
                  WHERE rebuild_id=?1 AND text_hash=?2"
            ),
            params![stage.id, hash.as_slice(), text],
            |row| row.get::<_, bool>(0),
        )
        .optional()?;
    if let Some(equal) = existing {
        return if equal {
            Ok(())
        } else {
            Err(QueryError::new(
                QueryErrorKind::Storage,
                "semantic rebuild detected a text hash collision",
            ))
        };
    }
    connection.execute(
        &format!(
            "INSERT INTO temp.{REBUILD_STAGE_TABLE}(\
        rebuild_id,text_hash,text_value,vector_blob) VALUES(?1,?2,?3,NULL)"
        ),
        params![stage.id, hash.as_slice(), text],
    )?;
    Ok(())
}

fn fill_rebuild_stage(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    stage: &RebuildStage,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let _cursor_memory = control.reserve(64)?;
    let mut cursor: Option<Vec<u8>> = None;
    let mut embedded_texts = 0_u64;
    loop {
        control.check()?;
        let batch = read_rebuild_text_batch(connection, stage, cursor.as_deref(), control)?;
        if batch.value.is_empty() {
            break;
        }
        cursor = batch.value.last().map(|(hash, _)| hash.clone());
        embedded_texts = embedded_texts.saturating_add(embed_rebuild_misses(
            connection,
            runtime,
            stage,
            &batch.value,
            control,
        )?);
    }
    Ok(embedded_texts)
}

fn embed_rebuild_misses(
    connection: &Connection,
    runtime: &ManagedRuntime<'_>,
    stage: &RebuildStage,
    misses: &[RebuildTextEntry],
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let _refs_memory = control.reserve(allocation_bytes::<[&str; 2]>(misses.len())?)?;
    let refs = misses
        .iter()
        .map(|(_, text)| text.as_str())
        .collect::<Vec<_>>();
    let _vectors_memory = control.reserve(checked_mul(
        vector_bytes(runtime.dimensions)?,
        misses.len() as u64,
    )?)?;
    control.check()?;
    let values = runtime
        .provider
        .embed_batch(&runtime.config_json, &refs, runtime.dimensions, &|| {
            control.check().is_err()
        })
        .map_err(map_provider_error)?;
    control.check()?;
    for ((hash, _), vector) in misses.iter().zip(values.chunks_exact(runtime.dimensions)) {
        control.check()?;
        validate_runtime_embedding(runtime, vector, control)?;
        let blob = encode_vector(vector, control)?;
        connection.execute(
            &format!(
                "UPDATE temp.{REBUILD_STAGE_TABLE} SET vector_blob=?3 \
                           WHERE rebuild_id=?1 AND text_hash=?2"
            ),
            params![stage.id, hash, &blob.value],
        )?;
    }
    Ok(misses.len() as u64)
}

fn read_rebuild_text_batch(
    connection: &Connection,
    stage: &RebuildStage,
    after: Option<&[u8]>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Budgeted<Vec<RebuildTextEntry>>> {
    let mut statement = connection.prepare(&format!(
        "SELECT text_hash,text_value FROM temp.{REBUILD_STAGE_TABLE} \
         WHERE rebuild_id=?1 AND vector_blob IS NULL AND (?2 IS NULL OR text_hash > ?2) \
         ORDER BY text_hash LIMIT ?3"
    ))?;
    let mut rows = statement.query(params![stage.id, after, REBUILD_TEXT_BATCH as i64])?;
    let mut batch = Buffer::new(control.resources())?;
    while let Some(row) = rows.next()? {
        control.check()?;
        let hash = row
            .get_ref(0)?
            .as_blob()
            .map_err(|_| QueryError::internal("semantic TEMP text hash is not a blob"))?;
        let text = row
            .get_ref(1)?
            .as_str()
            .map_err(|_| QueryError::internal("semantic TEMP source is not text"))?;
        let bytes = checked_add(hash.len() as u64, text.len() as u64)?;
        batch.reserve(bytes)?;
        batch.push(
            (hash.to_vec(), clone_string_controlled(text, Some(control))?),
            0,
        )?;
    }
    let (value, memory) = batch.into_parts();
    Ok(Budgeted { value, memory })
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

#[cfg(test)]
mod transition_tests;
