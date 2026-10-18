use std::collections::BTreeSet;

use lithograph_fts5::{
    Fts5Error, dual_tokenizer_spec, ensure_dual_tokenizer, override_query_tokenizer,
    validate_tokenizer,
};
use rusqlite::{Connection, OptionalExtension as _, params_from_iter};

use crate::cypher::{Value, VectorValues};
use crate::storage::{
    self, IndexConfiguration, IndexDefinition, IndexTarget, RelationshipRecord, SchemaState,
    Snapshot, StandardIndexKind,
};

use super::graph::{self, ResolvedGraphView};
use super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, checked_mul, memory_error,
};
use super::{QueryError, QueryErrorKind, QueryResult};

mod execution_resources;
mod hnsw;
use execution_resources::{
    fulltext_metadata_memory, fulltext_text_controlled, reserve_search_slot,
    reserve_vector_workspace, stored_vector_valid_controlled, try_vector_numbers_controlled,
    validate_query_vector_controlled,
};
pub(crate) use execution_resources::{
    reserve_managed_hnsw_workspace, search_index_metadata_bytes, sort_semantic_hits_controlled,
};
mod fulltext_translation;
mod similarity;
use fulltext_translation::translate_fulltext_query;
mod vector_search;
pub(crate) use vector_search::{vector_candidates_controlled, vector_initial_candidate_limit};
#[cfg(test)]
mod tests;
use hnsw::{
    ManagedHnswEntry, build_managed_vector_cache_controlled, invalidate_managed_vector_cache,
    query_managed_vector_cache_controlled,
};
#[cfg(test)]
use similarity::{has_finite_nonzero_norm, vector_similarity_numbers};
pub(crate) use similarity::{
    has_finite_nonzero_norm_controlled, vector_similarity_numbers_controlled,
};

const SEMANTIC_SCAN_PAGE_SIZE: usize = 4_096;

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum SemanticEntity {
    Node(i64),
    Relationship(RelationshipRecord),
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct SemanticHit {
    pub(crate) entity: SemanticEntity,
    pub(crate) score: f64,
}

pub(crate) struct VectorCandidateResult {
    pub(crate) hits: Vec<SemanticHit>,
    pub(crate) exhaustive: bool,
    pub(crate) memory: Option<MemoryReservation>,
}

pub(crate) fn build_managed_hnsw_cache(
    connection: &Connection,
    cache_key: &str,
    similarity: &str,
    entries: Vec<(i64, Vec<f32>)>,
    is_interrupted: &dyn Fn() -> bool,
    resources: &crate::query::resources::QueryResources,
) -> QueryResult<bool> {
    build_managed_vector_cache_controlled(
        connection,
        cache_key,
        similarity,
        entries
            .into_iter()
            .map(|(owner_id, vector)| ManagedHnswEntry { owner_id, vector })
            .collect(),
        is_interrupted,
        Some(resources),
    )
}

pub(crate) fn invalidate_managed_hnsw_cache(
    connection: &Connection,
    cache_key: &str,
) -> QueryResult<()> {
    invalidate_managed_vector_cache(connection, cache_key)
}

pub(crate) fn query_managed_hnsw_cache(
    connection: &Connection,
    cache: (&str, &crate::query::resources::QueryResources),
    query: &[f32],
    similarity: &str,
    candidate_limit: usize,
    is_interrupted: &dyn Fn() -> bool,
    resolve_entity: impl FnMut(i64) -> QueryResult<Option<SemanticEntity>>,
) -> QueryResult<Option<VectorCandidateResult>> {
    query_managed_vector_cache_controlled(
        connection,
        (cache.0, Some(cache.1)),
        query,
        similarity,
        candidate_limit,
        is_interrupted,
        resolve_entity,
    )
}

struct FullTextCandidate {
    owner_id: i64,
    score: f64,
}

struct VectorSearchInput<'a> {
    query: &'a [f32],
    similarity: &'a str,
}

pub(crate) struct FullTextQueryInput<'a> {
    pub(crate) relationship_query: bool,
    pub(crate) query: &'a str,
    pub(crate) skip: usize,
    pub(crate) limit: Option<usize>,
    pub(crate) analyzer: Option<&'a str>,
}

pub(crate) fn resolve_semantic_index(
    _connection: &Connection,
    snapshot: &Snapshot<'_>,
    name: &str,
    expected: StandardIndexKind,
) -> QueryResult<IndexDefinition> {
    let schema = snapshot.schema_state()?;
    let index = schema
        .indexes
        .get(name)
        .ok_or_else(|| QueryError::semantic(format!("Index {name} does not exist")))?;
    if index.kind != expected {
        return Err(QueryError::semantic(format!(
            "Index {name} is {}, not {}",
            super::schema::standard_index_kind_name(index.kind),
            super::schema::standard_index_kind_name(expected)
        )));
    }
    Ok(index.clone())
}

pub(crate) fn validate_query_index_access(
    index: &IndexDefinition,
    graph_view: &ResolvedGraphView,
) -> QueryResult<()> {
    if graph_view.query_access.is_empty() {
        return Ok(());
    }
    if index_properties(index)?
        .iter()
        .any(|property| graph_view.query_access.denies(property))
    {
        return Err(QueryError::access_denied(
            "Search cannot depend on a query-restricted graph property",
        ));
    }
    Ok(())
}

fn sort_semantic_hits_with_interrupt(
    hits: &mut [SemanticHit],
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let mut work = 0_u16;
    sort_semantic_hits_checked(hits, &mut || {
        work += 1;
        if work == 256 {
            work = 0;
            super::check_interrupted(is_interrupted)?;
        }
        Ok(())
    })?;
    super::check_interrupted(is_interrupted)
}

fn sort_semantic_hits_checked(
    hits: &mut [SemanticHit],
    checkpoint: &mut dyn FnMut() -> QueryResult<()>,
) -> QueryResult<()> {
    let mut error = None;
    hits.sort_by(|left, right| {
        if error.is_some() {
            return std::cmp::Ordering::Equal;
        }
        if let Err(failure) = checkpoint() {
            error = Some(failure);
            return std::cmp::Ordering::Equal;
        }
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| entity_id(left.entity).cmp(&entity_id(right.entity)))
    });
    error.map_or(Ok(()), Err)
}

fn vector_configuration(index: &IndexDefinition) -> QueryResult<(Option<usize>, &str)> {
    let Some(IndexConfiguration::Vector {
        dimensions,
        similarity_function,
        ..
    }) = &index.configuration
    else {
        return Err(QueryError::internal(
            "VECTOR Index is missing its versioned configuration",
        ));
    };
    let dimensions = dimensions
        .map(usize::try_from)
        .transpose()
        .map_err(|_| QueryError::internal("VECTOR Index dimension is too large"))?;
    Ok((dimensions, similarity_function))
}

fn vector_hit_controlled(
    index: &IndexDefinition,
    input: &VectorSearchInput<'_>,
    entity: SemanticEntity,
    value: Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<SemanticHit>> {
    let (vector, _vector_memory) = try_vector_numbers_controlled(&value, control)?;
    let Some(vector) = vector else {
        return Ok(None);
    };
    if !stored_vector_valid_controlled(&vector, index, control)?
        || vector.len() != input.query.len()
    {
        return Ok(None);
    }
    Ok(Some(SemanticHit {
        entity,
        score: similarity::vector_similarity_numbers_controlled(
            &vector,
            input.query,
            input.similarity,
            &|| control.is_some_and(|control| control.check().is_err()),
        )?,
    }))
}

pub(crate) fn visit_indexed_entities(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    graph_view: Option<&ResolvedGraphView>,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
    mut visit: impl FnMut(SemanticEntity) -> QueryResult<()>,
) -> QueryResult<()> {
    let membership = semantic_membership(connection, index)?;
    match membership {
        SemanticMembership::NodeLabels(labels) => visit_semantic_nodes(
            snapshot,
            &labels,
            graph_view,
            is_interrupted,
            control,
            &mut visit,
        ),
        SemanticMembership::RelationshipTypes(types) => visit_semantic_relationships(
            snapshot,
            &types,
            graph_view,
            is_interrupted,
            control,
            &mut visit,
        ),
    }
}

pub(crate) enum SemanticMembership {
    NodeLabels(BTreeSet<i64>),
    RelationshipTypes(BTreeSet<i64>),
}

pub(crate) fn semantic_membership(
    connection: &Connection,
    index: &IndexDefinition,
) -> QueryResult<SemanticMembership> {
    match &index.target {
        IndexTarget::NodeProperties { .. } => Ok(SemanticMembership::NodeLabels(
            resolve_label_ids(connection, &index.labels_or_types)?,
        )),
        IndexTarget::RelationshipProperties { .. } => Ok(SemanticMembership::RelationshipTypes(
            resolve_relationship_type_ids(connection, &index.labels_or_types)?,
        )),
        IndexTarget::NodeLookup | IndexTarget::RelationshipLookup => {
            Err(QueryError::internal("semantic Index has a lookup target"))
        }
    }
}

fn visit_semantic_nodes(
    snapshot: &Snapshot<'_>,
    labels: &BTreeSet<i64>,
    graph_view: Option<&ResolvedGraphView>,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
    visit: &mut impl FnMut(SemanticEntity) -> QueryResult<()>,
) -> QueryResult<()> {
    for label_id in labels {
        let mut after = 0_i64;
        loop {
            let page = snapshot.scan_label_after(*label_id, after, SEMANTIC_SCAN_PAGE_SIZE)?;
            for node in page.items {
                if is_interrupted() {
                    return Err(QueryError::interrupted());
                }
                if labels.len() > 1
                    && first_matching_label(snapshot, node, labels)? != Some(*label_id)
                {
                    continue;
                }
                visit_visible_entity(
                    snapshot,
                    graph_view,
                    SemanticEntity::Node(node),
                    control,
                    visit,
                )?;
            }
            let Some(next_after) = page.next_after else {
                break;
            };
            after = next_after;
        }
    }
    Ok(())
}

fn visit_semantic_relationships(
    snapshot: &Snapshot<'_>,
    types: &BTreeSet<i64>,
    graph_view: Option<&ResolvedGraphView>,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
    visit: &mut impl FnMut(SemanticEntity) -> QueryResult<()>,
) -> QueryResult<()> {
    for type_id in types {
        let mut after = 0_i64;
        loop {
            let page =
                snapshot.scan_relationship_type_after(*type_id, after, SEMANTIC_SCAN_PAGE_SIZE)?;
            for relationship in page.items {
                if is_interrupted() {
                    return Err(QueryError::interrupted());
                }
                visit_visible_entity(
                    snapshot,
                    graph_view,
                    SemanticEntity::Relationship(relationship),
                    control,
                    visit,
                )?;
            }
            let Some(next_after) = page.next_after else {
                break;
            };
            after = next_after;
        }
    }
    Ok(())
}

fn first_matching_label(
    snapshot: &Snapshot<'_>,
    node: i64,
    labels: &BTreeSet<i64>,
) -> QueryResult<Option<i64>> {
    Ok(snapshot
        .labels(node)?
        .into_iter()
        .filter(|label| labels.contains(label))
        .min())
}

fn visit_visible_entity(
    snapshot: &Snapshot<'_>,
    graph_view: Option<&ResolvedGraphView>,
    entity: SemanticEntity,
    control: Option<&ExecutionControl<'_>>,
    visit: &mut impl FnMut(SemanticEntity) -> QueryResult<()>,
) -> QueryResult<()> {
    if let Some(view) = graph_view
        && !semantic_entity_visible(snapshot, view, entity, control)?
    {
        return Ok(());
    }
    visit(entity)
}

pub(crate) fn semantic_entity_matches(
    snapshot: &Snapshot<'_>,
    entity: SemanticEntity,
    membership: &SemanticMembership,
) -> QueryResult<bool> {
    match (entity, membership) {
        (SemanticEntity::Node(node), SemanticMembership::NodeLabels(labels)) => Ok(snapshot
            .labels(node)?
            .into_iter()
            .any(|label| labels.contains(&label))),
        (
            SemanticEntity::Relationship(relationship),
            SemanticMembership::RelationshipTypes(types),
        ) => Ok(types.contains(&relationship.type_id)),
        _ => Ok(false),
    }
}

pub(crate) fn semantic_entity_visible(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    entity: SemanticEntity,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    match (entity, control) {
        (SemanticEntity::Node(node), Some(control)) => {
            graph_view.visible_node_controlled(snapshot, node, control)
        }
        (SemanticEntity::Relationship(relationship), Some(control)) => {
            graph_view.visible_relationship_controlled(snapshot, relationship, control)
        }
        (SemanticEntity::Node(node), None) => graph_view.visible_node(snapshot, node),
        (SemanticEntity::Relationship(relationship), None) => {
            graph_view.visible_relationship(snapshot, relationship)
        }
    }
}

pub(crate) fn semantic_property(
    snapshot: &Snapshot<'_>,
    entity: SemanticEntity,
    property: &str,
) -> QueryResult<Value> {
    match entity {
        SemanticEntity::Node(node) => graph::node_property(snapshot, node, property),
        SemanticEntity::Relationship(relationship) => {
            graph::relationship_property(snapshot, relationship.id, property)
        }
    }
}

pub(crate) fn semantic_property_controlled(
    snapshot: &Snapshot<'_>,
    entity: SemanticEntity,
    property: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(Value, Option<MemoryReservation>)> {
    let Some(control) = control else {
        return semantic_property(snapshot, entity, property).map(|value| (value, None));
    };
    let access = super::access::PropertyRules::default();
    let (value, memory) = match entity {
        SemanticEntity::Node(node) => {
            graph::node_property_with_control(snapshot, node, property, &access, control)?
        }
        SemanticEntity::Relationship(relationship) => graph::relationship_property_with_control(
            snapshot,
            relationship.id,
            property,
            &access,
            control,
        )?,
    };
    Ok((value, Some(memory)))
}

fn resolve_label_ids(connection: &Connection, names: &[String]) -> QueryResult<BTreeSet<i64>> {
    resolve_dictionary_ids(connection, names, storage::find_label)
}

fn resolve_relationship_type_ids(
    connection: &Connection,
    names: &[String],
) -> QueryResult<BTreeSet<i64>> {
    resolve_dictionary_ids(connection, names, storage::find_relationship_type)
}

fn resolve_dictionary_ids(
    connection: &Connection,
    names: &[String],
    mut resolve: impl FnMut(&Connection, &str) -> storage::StorageResult<Option<i64>>,
) -> QueryResult<BTreeSet<i64>> {
    let mut ids = BTreeSet::new();
    for name in names {
        if let Some(id) = resolve(connection, name)? {
            ids.insert(id);
        }
    }
    Ok(ids)
}

pub(crate) fn entity_id(entity: SemanticEntity) -> i64 {
    match entity {
        SemanticEntity::Node(id) => id,
        SemanticEntity::Relationship(relationship) => relationship.id,
    }
}

pub(crate) fn fulltext_query_controlled(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &FullTextQueryInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<VectorCandidateResult> {
    fulltext_query_inner(
        connection,
        snapshot,
        graph_view,
        index,
        input,
        is_interrupted,
        Some(control),
    )
}

fn fulltext_query_inner(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &FullTextQueryInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<VectorCandidateResult> {
    let _metadata_memory = fulltext_metadata_memory(index, input.query, control)?;
    validate_query_index_access(index, graph_view)?;
    validate_fulltext_target(index, input.relationship_query)?;
    let configured_analyzer = fulltext_configuration(index)?;
    if let Some(analyzer) = input.analyzer
        && analyzer != configured_analyzer
    {
        return fulltext_query_with_analyzer_override(
            connection,
            snapshot,
            graph_view,
            index,
            input,
            is_interrupted,
            control,
        );
    }
    let table = ensure_fulltext_cache(connection, snapshot, index, is_interrupted, control)?;
    let (candidates, _candidates_memory) =
        default_fulltext_candidates(connection, index, input.query, &table, control)?;
    visible_fulltext_hits(
        snapshot,
        graph_view,
        input,
        candidates,
        is_interrupted,
        control,
    )
}

fn fulltext_query_with_analyzer_override(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &FullTextQueryInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<VectorCandidateResult> {
    let analyzer = input
        .analyzer
        .ok_or_else(|| QueryError::internal("full-text analyzer override is missing"))?;
    let configured = fulltext_configuration(index)?;
    ensure_dual_tokenizer(connection).map_err(|error| {
        map_fts5_provider_error(
            &index.name,
            "query analyzer adapter registration",
            QueryErrorKind::Semantic,
            error,
        )
    })?;
    let table = ensure_fulltext_override_cache(
        connection,
        snapshot,
        index,
        configured,
        is_interrupted,
        control,
    )?;
    let _override_guard =
        override_query_tokenizer(connection, &table, analyzer).map_err(|error| {
            map_fts5_provider_error(
                &index.name,
                "query analyzer construction",
                QueryErrorKind::Semantic,
                error,
            )
        })?;
    let (candidates, _candidates_memory) =
        default_fulltext_candidates(connection, index, input.query, &table, control)?;
    visible_fulltext_hits(
        snapshot,
        graph_view,
        input,
        candidates,
        is_interrupted,
        control,
    )
}

fn default_fulltext_candidates(
    connection: &Connection,
    index: &IndexDefinition,
    query: &str,
    table: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(Vec<FullTextCandidate>, Option<MemoryReservation>)> {
    let translated = translate_fulltext_query(index, query, control)?;
    if translated.is_empty() {
        return Ok((Vec::new(), None));
    }
    let sql = format!(
        "SELECT owner_id, bm25({table}) FROM temp.{table} WHERE {table} MATCH ?1 ORDER BY bm25({table}), owner_id"
    );
    let mut statement = connection.prepare(&sql).map_err(|error| {
        map_fulltext_sqlite_error(
            &index.name,
            "query expression",
            QueryErrorKind::Semantic,
            error,
        )
    })?;
    let rows = statement
        .query_map([translated], |row| {
            let raw_score = row.get::<_, f64>(1)?;
            Ok(FullTextCandidate {
                owner_id: row.get(0)?,
                score: normalize_fulltext_score(raw_score),
            })
        })
        .map_err(|error| {
            map_fulltext_sqlite_error(
                &index.name,
                "query expression",
                QueryErrorKind::Semantic,
                error,
            )
        })?;
    let mut output = Vec::new();
    let mut memory = control.map(|control| control.reserve(0)).transpose()?;
    for row in rows {
        if let Some(control) = control {
            control.work(1)?;
        }
        let row = row.map_err(|error| {
            map_fulltext_sqlite_error(
                &index.name,
                "query expression",
                QueryErrorKind::Semantic,
                error,
            )
        })?;
        reserve_search_slot(&mut output, &mut memory)?;
        output.push(row);
    }
    Ok((output, memory))
}

fn normalize_fulltext_score(raw_score: f64) -> f64 {
    if raw_score <= 0.0 {
        -raw_score
    } else {
        1.0 / (1.0 + raw_score)
    }
}

fn visible_fulltext_hits(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    input: &FullTextQueryInput<'_>,
    candidates: Vec<FullTextCandidate>,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<VectorCandidateResult> {
    let mut visible_seen = 0_usize;
    let mut hits = Vec::new();
    let mut memory = control.map(|control| control.reserve(0)).transpose()?;
    for candidate in candidates {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        let Some(entity) = visible_fulltext_entity(
            snapshot,
            graph_view,
            input.relationship_query,
            candidate.owner_id,
            control,
        )?
        else {
            continue;
        };
        if visible_seen < input.skip {
            visible_seen += 1;
            continue;
        }
        if input.limit.is_some_and(|limit| hits.len() >= limit) {
            break;
        }
        reserve_search_slot(&mut hits, &mut memory)?;
        hits.push(SemanticHit {
            entity,
            score: candidate.score,
        });
    }
    Ok(VectorCandidateResult {
        hits,
        exhaustive: true,
        memory,
    })
}

fn visible_fulltext_entity(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    relationship_query: bool,
    owner_id: i64,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<SemanticEntity>> {
    let entity = if relationship_query {
        let Some(relationship) = snapshot.relationship(owner_id)? else {
            return Ok(None);
        };
        SemanticEntity::Relationship(relationship)
    } else {
        SemanticEntity::Node(owner_id)
    };
    Ok(semantic_entity_visible(snapshot, graph_view, entity, control)?.then_some(entity))
}

pub(crate) fn validate_fulltext_schema_specification(analyzer: &str) -> QueryResult<()> {
    validate_fulltext_specification(analyzer, QueryErrorKind::Schema)
}

pub(crate) fn validate_fulltext_query_specification(analyzer: &str) -> QueryResult<()> {
    validate_fulltext_specification(analyzer, QueryErrorKind::Semantic)
}

fn validate_fulltext_specification(analyzer: &str, kind: QueryErrorKind) -> QueryResult<()> {
    if analyzer.contains('\0') {
        return Err(QueryError::new(
            kind,
            "full-text analyzer specification cannot contain NUL",
        ));
    }
    if analyzer.is_empty() || analyzer.bytes().all(|byte| byte == b' ') {
        return Err(QueryError::new(
            kind,
            "full-text analyzer specification cannot be empty",
        ));
    }
    Ok(())
}

fn validate_fulltext_target(index: &IndexDefinition, relationship_query: bool) -> QueryResult<()> {
    let target_is_relationship = matches!(index.target, IndexTarget::RelationshipProperties { .. });
    if relationship_query != target_is_relationship {
        let expected = if relationship_query {
            "Relationship"
        } else {
            "Node"
        };
        return Err(QueryError::semantic(format!(
            "FULLTEXT Index {} does not target {expected}s",
            index.name
        )));
    }
    Ok(())
}

fn fulltext_configuration(index: &IndexDefinition) -> QueryResult<&str> {
    match &index.configuration {
        Some(IndexConfiguration::FullText { analyzer, .. }) => Ok(analyzer),
        _ => Err(QueryError::internal(
            "FULLTEXT Index is missing its versioned configuration",
        )),
    }
}

pub(crate) fn validate_fulltext_transition_controlled(
    connection: &Connection,
    previous: &SchemaState,
    next: &SchemaState,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let check = || super::check_interrupted(is_interrupted).map_err(QueryError::into_storage_error);
    super::check_interrupted(is_interrupted)?;
    for (name, index) in &next.indexes {
        super::check_interrupted(is_interrupted)?;
        if index.kind != StandardIndexKind::FullText
            || previous
                .indexes
                .get(name)
                .map(|previous| previous.canonical_eq_checked(index, &check))
                .transpose()?
                .unwrap_or(false)
        {
            continue;
        }
        let analyzer = fulltext_configuration(index)?;
        validate_fulltext_schema_specification(analyzer)?;
        super::check_interrupted(is_interrupted)?;
        probe_fulltext_specification(connection, index, analyzer)?;
        super::check_interrupted(is_interrupted)?;
    }
    Ok(())
}

fn probe_fulltext_specification(
    connection: &Connection,
    index: &IndexDefinition,
    analyzer: &str,
) -> QueryResult<()> {
    validate_tokenizer(connection, analyzer).map_err(|error| {
        map_fts5_provider_error(
            &index.name,
            "schema analyzer construction",
            QueryErrorKind::Schema,
            error,
        )
    })
}

fn ensure_fulltext_cache(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<String> {
    let table = fulltext_cache_table(snapshot, index)?;
    let slot = prepare_fulltext_cache_slot(connection, &table, &index.name)?;
    if slot == FullTextCacheSlot::Ready {
        return Ok(table);
    }
    build_fulltext_cache(
        connection,
        snapshot,
        index,
        &table,
        slot == FullTextCacheSlot::Missing,
        is_interrupted,
        control,
    )?;
    Ok(table)
}

fn fulltext_cache_table(snapshot: &Snapshot<'_>, index: &IndexDefinition) -> QueryResult<String> {
    let digest = semantic_cache_digest(snapshot, index, b"LITHOGRAPH_FTS5_CACHE_V2")?;
    Ok(format!("_lithograph_fts_{}", &digest[..24]))
}

fn ensure_fulltext_override_cache(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    configured_analyzer: &str,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<String> {
    let digest = semantic_cache_digest(snapshot, index, b"LITHOGRAPH_FTS5_OVERRIDE_V2")?;
    let table = format!("_lithograph_fts_override_{}", &digest[..24]);
    let slot = prepare_fulltext_cache_slot(connection, &table, &index.name)?;
    if slot == FullTextCacheSlot::Ready {
        return Ok(table);
    }
    let tokenizer = dual_tokenizer_spec(configured_analyzer, configured_analyzer, &table);
    build_fulltext_table(
        connection,
        snapshot,
        index,
        (&table, slot == FullTextCacheSlot::Missing),
        &tokenizer,
        is_interrupted,
        control,
    )?;
    Ok(table)
}

fn build_fulltext_cache(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    table: &str,
    create_table: bool,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    let analyzer = fulltext_configuration(index)?;
    build_fulltext_table(
        connection,
        snapshot,
        index,
        (table, create_table),
        analyzer,
        is_interrupted,
        control,
    )
}

fn build_fulltext_table(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    table: (&str, bool),
    analyzer: &str,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    let (table, create_table) = table;
    let properties = index_properties(index)?;
    let columns = (0..properties.len())
        .map(|index| format!("c{index}"))
        .collect::<Vec<_>>();
    let create = format!(
        "CREATE VIRTUAL TABLE temp.{table} USING fts5({}, owner_id UNINDEXED, tokenize={})",
        columns.join(", "),
        sql_text_literal(analyzer)
    );
    if create_table {
        connection.execute_batch(&create).map_err(|error| {
            map_fulltext_sqlite_error(
                &index.name,
                "query analyzer construction",
                QueryErrorKind::Semantic,
                error,
            )
        })?;
    }
    let result = populate_fulltext_cache(
        connection,
        snapshot,
        index,
        table,
        properties,
        is_interrupted,
        control,
    );
    if let Err(error) = result {
        return cleanup_failed_fulltext_build(connection, table, &index.name, error);
    }
    if let Err(error) = mark_fulltext_cache_ready(connection, table) {
        return cleanup_failed_fulltext_build(connection, table, &index.name, error);
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum FullTextCacheSlot {
    Ready,
    EmptyExisting,
    Missing,
}

fn prepare_fulltext_cache_slot(
    connection: &Connection,
    table: &str,
    index_name: &str,
) -> QueryResult<FullTextCacheSlot> {
    ensure_fulltext_ready_table(connection)?;
    let exists = connection
        .query_row(
            "SELECT 1 FROM temp.sqlite_schema WHERE type = 'table' AND name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    let ready = fulltext_cache_ready(connection, table)?;
    if exists && ready {
        return Ok(FullTextCacheSlot::Ready);
    }
    clear_fulltext_cache_ready(connection, table)?;
    if exists {
        reset_fulltext_table(connection, table, index_name, "discard incomplete cache")?;
        return Ok(FullTextCacheSlot::EmptyExisting);
    }
    Ok(FullTextCacheSlot::Missing)
}

fn ensure_fulltext_ready_table(connection: &Connection) -> QueryResult<()> {
    connection.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _lithograph_semantic_fts_ready(\
             table_name TEXT PRIMARY KEY\
         ) WITHOUT ROWID;",
    )?;
    Ok(())
}

fn fulltext_cache_ready(connection: &Connection, table: &str) -> QueryResult<bool> {
    Ok(connection
        .query_row(
            "SELECT 1 FROM temp._lithograph_semantic_fts_ready WHERE table_name = ?1",
            [table],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some())
}

fn mark_fulltext_cache_ready(connection: &Connection, table: &str) -> QueryResult<()> {
    connection.execute(
        "INSERT OR REPLACE INTO temp._lithograph_semantic_fts_ready(table_name) VALUES(?1)",
        [table],
    )?;
    Ok(())
}

fn clear_fulltext_cache_ready(connection: &Connection, table: &str) -> QueryResult<()> {
    connection.execute(
        "DELETE FROM temp._lithograph_semantic_fts_ready WHERE table_name = ?1",
        [table],
    )?;
    Ok(())
}

fn cleanup_failed_fulltext_build(
    connection: &Connection,
    table: &str,
    index_name: &str,
    error: QueryError,
) -> QueryResult<()> {
    let _ = clear_fulltext_cache_ready(connection, table);
    match reset_fulltext_table(connection, table, index_name, "cache build rollback") {
        Ok(()) => Err(error),
        Err(cleanup_error) if cleanup_error.kind == QueryErrorKind::Busy => {
            // The missing ready mark quarantines this table. A later use will
            // retry the DML reset before any result can be read from it.
            Err(error)
        }
        Err(cleanup_error) => Err(cleanup_error),
    }
}

fn reset_fulltext_table(
    connection: &Connection,
    table: &str,
    index_name: &str,
    stage: &str,
) -> QueryResult<()> {
    connection
        .execute(&format!("DELETE FROM temp.{table}"), [])
        .map(|_| ())
        .map_err(|error| {
            map_fulltext_sqlite_error(index_name, stage, QueryErrorKind::Internal, error)
        })
}

fn sql_text_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn map_fts5_provider_error(
    index_name: &str,
    stage: &str,
    fallback: QueryErrorKind,
    error: Fts5Error,
) -> QueryError {
    fulltext_provider_error(index_name, stage, fallback, error.sqlite_code)
}

fn map_fulltext_sqlite_error(
    index_name: &str,
    stage: &str,
    fallback: QueryErrorKind,
    error: rusqlite::Error,
) -> QueryError {
    let mapped = QueryError::from(error);
    if matches!(
        mapped.kind,
        QueryErrorKind::Busy
            | QueryErrorKind::Io
            | QueryErrorKind::Resource
            | QueryErrorKind::Interrupted
    ) {
        return mapped;
    }
    let code = mapped.sqlite_code.unwrap_or(rusqlite::ffi::SQLITE_ERROR);
    fulltext_provider_error(index_name, stage, fallback, code)
}

fn fulltext_provider_error(
    index_name: &str,
    stage: &str,
    fallback: QueryErrorKind,
    sqlite_code: i32,
) -> QueryError {
    let kind = match sqlite_code & 0xff {
        rusqlite::ffi::SQLITE_INTERRUPT => QueryErrorKind::Interrupted,
        rusqlite::ffi::SQLITE_BUSY | rusqlite::ffi::SQLITE_LOCKED => QueryErrorKind::Busy,
        rusqlite::ffi::SQLITE_IOERR
        | rusqlite::ffi::SQLITE_CANTOPEN
        | rusqlite::ffi::SQLITE_READONLY => QueryErrorKind::Io,
        rusqlite::ffi::SQLITE_NOMEM | rusqlite::ffi::SQLITE_FULL | rusqlite::ffi::SQLITE_TOOBIG => {
            QueryErrorKind::Resource
        }
        _ => fallback,
    };
    let mut error = QueryError::new(kind, format!("FULLTEXT Index {index_name} {stage} failed"));
    error.sqlite_code = Some(sqlite_code);
    error
}

fn index_properties(index: &IndexDefinition) -> QueryResult<&[String]> {
    match &index.target {
        IndexTarget::NodeProperties { properties, .. }
        | IndexTarget::RelationshipProperties { properties, .. } => Ok(properties),
        IndexTarget::NodeLookup | IndexTarget::RelationshipLookup => {
            Err(QueryError::internal("semantic Index has a lookup target"))
        }
    }
}

fn populate_fulltext_cache(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    table: &str,
    properties: &[String],
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    let columns = (0..properties.len())
        .map(|index| format!("c{index}"))
        .collect::<Vec<_>>();
    let placeholders = (1..=properties.len() + 1)
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>();
    let sql = format!(
        "INSERT INTO temp.{table}({}, owner_id) VALUES({})",
        columns.join(", "),
        placeholders.join(", ")
    );
    visit_indexed_entities(
        connection,
        snapshot,
        index,
        None,
        is_interrupted,
        control,
        |entity| {
            insert_fulltext_values(
                connection,
                &sql,
                &index.name,
                entity_id(entity),
                properties,
                |property| semantic_property_controlled(snapshot, entity, property, control),
                control,
            )?;
            Ok(())
        },
    )
}

fn insert_fulltext_values(
    connection: &Connection,
    sql: &str,
    index_name: &str,
    owner_id: i64,
    properties: &[String],
    mut property_value: impl FnMut(&str) -> QueryResult<(Value, Option<MemoryReservation>)>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    let mut memory = control
        .map(|control| {
            control.reserve(allocation_bytes::<rusqlite::types::Value>(
                properties.len() + 1,
            )?)
        })
        .transpose()?;
    let mut values = Vec::with_capacity(properties.len() + 1);
    let mut indexed = false;
    for property in properties {
        if let Some(control) = control {
            control.work(1)?;
        }
        let (value, _property_memory) = property_value(property)?;
        let _joining = control
            .map(|control| {
                control.reserve(checked_mul(
                    super::resources::value_bytes_controlled(&value, control)?,
                    4,
                )?)
            })
            .transpose()?;
        let text = fulltext_text_controlled(value, control)?;
        indexed |= text.is_some();
        if let Some(memory) = &mut memory {
            memory.grow(text.as_ref().map_or(0, |text| text.capacity() as u64))?;
        }
        values.push(rusqlite::types::Value::Text(text.unwrap_or_default()));
    }
    if indexed {
        values.push(rusqlite::types::Value::Integer(owner_id));
        connection
            .execute(sql, params_from_iter(values))
            .map_err(|error| {
                map_fulltext_sqlite_error(
                    index_name,
                    "document tokenization",
                    QueryErrorKind::Semantic,
                    error,
                )
            })?;
    }
    Ok(())
}

pub(crate) fn semantic_cache_digest(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    namespace: &[u8],
) -> QueryResult<String> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(namespace);
    hasher.update(snapshot.cache_identity().as_bytes());
    let encoded = serde_json::to_vec(index).map_err(|error| {
        QueryError::internal(format!("failed to encode Index definition: {error}"))
    })?;
    hasher.update(&encoded);
    Ok(hasher.finalize().to_hex().to_string())
}
