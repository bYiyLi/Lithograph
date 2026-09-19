use std::collections::BTreeSet;

use lithograph_fts5::{
    Fts5Error, dual_tokenizer_spec, ensure_dual_tokenizer, override_query_tokenizer,
};
use rusqlite::{Connection, OptionalExtension as _, params_from_iter};

use crate::cypher::{Value, VectorValues};
use crate::storage::{
    self, IndexConfiguration, IndexDefinition, IndexTarget, RelationshipRecord, SchemaState,
    Snapshot, StandardIndexKind,
};

use super::graph::{self, ResolvedGraphView};
use super::{QueryError, QueryErrorKind, QueryResult};

mod hnsw;
mod similarity;
#[cfg(test)]
mod tests;
use hnsw::{
    ManagedHnswEntry, build_managed_vector_cache, build_vector_cache,
    invalidate_managed_vector_cache, query_managed_vector_cache, query_vector_cache,
};
pub(crate) use similarity::{has_finite_nonzero_norm, vector_similarity_numbers};

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
}

pub(crate) fn build_managed_hnsw_cache(
    connection: &Connection,
    cache_key: &str,
    similarity: &str,
    entries: Vec<(i64, Vec<f32>)>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<bool> {
    build_managed_vector_cache(
        connection,
        cache_key,
        similarity,
        entries
            .into_iter()
            .map(|(owner_id, vector)| ManagedHnswEntry { owner_id, vector })
            .collect(),
        is_interrupted,
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
    cache_key: &str,
    query: &[f32],
    similarity: &str,
    candidate_limit: usize,
    is_interrupted: &dyn Fn() -> bool,
    resolve_entity: impl FnMut(i64) -> QueryResult<Option<SemanticEntity>>,
) -> QueryResult<Option<VectorCandidateResult>> {
    query_managed_vector_cache(
        connection,
        cache_key,
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

pub(crate) fn vector_candidates(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    query: &Value,
    candidate_limit: usize,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<VectorCandidateResult> {
    let query_vector = vector_numbers(query)?;
    let similarity = validate_vector_search_input(index, &query_vector)?;
    let input = VectorSearchInput {
        query: &query_vector,
        similarity,
    };
    if let Some(mut result) = query_vector_cache(
        snapshot,
        graph_view,
        index,
        query,
        candidate_limit,
        is_interrupted,
    )? {
        sort_semantic_hits(&mut result.hits);
        return Ok(result);
    }
    let mut hits = scan_vector_candidates(
        connection,
        snapshot,
        graph_view,
        index,
        &input,
        is_interrupted,
    )?;
    sort_semantic_hits(&mut hits);
    build_vector_cache(
        connection,
        snapshot,
        index,
        query_vector.len(),
        is_interrupted,
    )?;
    Ok(VectorCandidateResult {
        hits,
        exhaustive: true,
    })
}

pub(crate) fn vector_initial_candidate_limit(
    index: &IndexDefinition,
    requested: usize,
) -> QueryResult<usize> {
    let Some(IndexConfiguration::Vector {
        default_search_expansion_factor,
        ..
    }) = &index.configuration
    else {
        return Err(QueryError::internal(
            "VECTOR Index is missing its versioned configuration",
        ));
    };
    let expansion = default_search_expansion_factor
        .parse::<f64>()
        .map_err(|_| {
            QueryError::internal("VECTOR Index has an invalid persisted search expansion factor")
        })?;
    let expanded = (requested as f64 * expansion).ceil();
    if !expanded.is_finite() || expanded > usize::MAX as f64 {
        return Ok(usize::MAX);
    }
    Ok(requested.max(expanded as usize).max(1))
}

fn validate_vector_search_input<'a>(
    index: &'a IndexDefinition,
    query_vector: &[f32],
) -> QueryResult<&'a str> {
    let (dimensions, similarity) = vector_configuration(index)?;
    if let Some(dimensions) = dimensions
        && query_vector.len() != dimensions
    {
        return Err(QueryError::semantic(format!(
            "SEARCH vector dimension {} does not match Index dimension {dimensions}",
            query_vector.len()
        )));
    }
    validate_query_vector(query_vector, similarity)?;
    Ok(similarity)
}

fn scan_vector_candidates(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &VectorSearchInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<SemanticHit>> {
    let property = index_properties(index)?
        .first()
        .ok_or_else(|| QueryError::internal("VECTOR Index is missing its indexed property"))?;
    let mut hits = Vec::new();
    visit_indexed_entities(
        connection,
        snapshot,
        index,
        Some(graph_view),
        is_interrupted,
        |entity| {
            let value = semantic_property(snapshot, entity, property)?;
            if let Some(hit) = vector_hit(index, input, entity, value)? {
                hits.push(hit);
            }
            Ok(())
        },
    )?;
    Ok(hits)
}

pub(crate) fn sort_semantic_hits(hits: &mut [SemanticHit]) {
    hits.sort_by(|left, right| {
        right
            .score
            .total_cmp(&left.score)
            .then_with(|| entity_id(left.entity).cmp(&entity_id(right.entity)))
    });
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

fn vector_hit(
    index: &IndexDefinition,
    input: &VectorSearchInput<'_>,
    entity: SemanticEntity,
    value: Value,
) -> QueryResult<Option<SemanticHit>> {
    let Some(vector) = try_vector_numbers(&value)? else {
        return Ok(None);
    };
    if !stored_vector_valid(&vector, index)? || vector.len() != input.query.len() {
        return Ok(None);
    }
    Ok(Some(SemanticHit {
        entity,
        score: vector_similarity_numbers(&vector, input.query, input.similarity)?,
    }))
}

pub(crate) fn visit_indexed_entities(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    graph_view: Option<&ResolvedGraphView>,
    is_interrupted: &dyn Fn() -> bool,
    mut visit: impl FnMut(SemanticEntity) -> QueryResult<()>,
) -> QueryResult<()> {
    let membership = semantic_membership(connection, index)?;
    match membership {
        SemanticMembership::NodeLabels(labels) => {
            visit_semantic_nodes(snapshot, &labels, graph_view, is_interrupted, &mut visit)
        }
        SemanticMembership::RelationshipTypes(types) => {
            visit_semantic_relationships(snapshot, &types, graph_view, is_interrupted, &mut visit)
        }
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
                visit_visible_entity(snapshot, graph_view, SemanticEntity::Node(node), visit)?;
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
    visit: &mut impl FnMut(SemanticEntity) -> QueryResult<()>,
) -> QueryResult<()> {
    if let Some(view) = graph_view
        && !semantic_entity_visible(snapshot, view, entity)?
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
) -> QueryResult<bool> {
    match entity {
        SemanticEntity::Node(node) => graph_view.visible_node(snapshot, node),
        SemanticEntity::Relationship(relationship) => {
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

pub(crate) fn fulltext_query(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &FullTextQueryInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<SemanticHit>> {
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
        );
    }
    let table = ensure_fulltext_cache(connection, snapshot, index, is_interrupted)?;
    let candidates = default_fulltext_candidates(connection, index, input.query, &table)?;
    visible_fulltext_hits(snapshot, graph_view, input, candidates, is_interrupted)
}

fn fulltext_query_with_analyzer_override(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &FullTextQueryInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<SemanticHit>> {
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
    let table =
        ensure_fulltext_override_cache(connection, snapshot, index, configured, is_interrupted)?;
    let _override_guard =
        override_query_tokenizer(connection, &table, analyzer).map_err(|error| {
            map_fts5_provider_error(
                &index.name,
                "query analyzer construction",
                QueryErrorKind::Semantic,
                error,
            )
        })?;
    let candidates = default_fulltext_candidates(connection, index, input.query, &table)?;
    visible_fulltext_hits(snapshot, graph_view, input, candidates, is_interrupted)
}

fn default_fulltext_candidates(
    connection: &Connection,
    index: &IndexDefinition,
    query: &str,
    table: &str,
) -> QueryResult<Vec<FullTextCandidate>> {
    let translated = translate_fulltext_query(index, query);
    if translated.is_empty() {
        return Ok(Vec::new());
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
        })?
        .collect::<Result<Vec<_>, _>>();
    rows.map_err(|error| {
        map_fulltext_sqlite_error(
            &index.name,
            "query expression",
            QueryErrorKind::Semantic,
            error,
        )
    })
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
) -> QueryResult<Vec<SemanticHit>> {
    let mut visible_seen = 0_usize;
    let mut hits = Vec::new();
    for candidate in candidates {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        let Some(entity) = visible_fulltext_entity(
            snapshot,
            graph_view,
            input.relationship_query,
            candidate.owner_id,
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
        hits.push(SemanticHit {
            entity,
            score: candidate.score,
        });
    }
    Ok(hits)
}

fn visible_fulltext_entity(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    relationship_query: bool,
    owner_id: i64,
) -> QueryResult<Option<SemanticEntity>> {
    if relationship_query {
        let Some(relationship) = snapshot.relationship(owner_id)? else {
            return Ok(None);
        };
        if graph_view.visible_relationship(snapshot, relationship)? {
            Ok(Some(SemanticEntity::Relationship(relationship)))
        } else {
            Ok(None)
        }
    } else if graph_view.visible_node(snapshot, owner_id)? {
        Ok(Some(SemanticEntity::Node(owner_id)))
    } else {
        Ok(None)
    }
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

pub(crate) fn validate_fulltext_transition(
    connection: &Connection,
    previous: &SchemaState,
    next: &SchemaState,
) -> QueryResult<()> {
    for (name, index) in &next.indexes {
        if index.kind != StandardIndexKind::FullText || previous.indexes.get(name) == Some(index) {
            continue;
        }
        let analyzer = fulltext_configuration(index)?;
        validate_fulltext_schema_specification(analyzer)?;
        probe_fulltext_specification(connection, index, analyzer)?;
    }
    Ok(())
}

fn probe_fulltext_specification(
    connection: &Connection,
    index: &IndexDefinition,
    analyzer: &str,
) -> QueryResult<()> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_FTS5_SCHEMA_PROBE_V1");
    hasher.update(index.name.as_bytes());
    hasher.update(analyzer.as_bytes());
    let digest = hasher.finalize().to_hex().to_string();
    let table = format!("_lithograph_fts_probe_{}", &digest[..20]);
    let create = format!(
        "CREATE VIRTUAL TABLE temp.{table} USING fts5(value, tokenize={})",
        sql_text_literal(analyzer)
    );
    connection.execute_batch(&create).map_err(|error| {
        map_fulltext_sqlite_error(
            &index.name,
            "schema analyzer construction",
            QueryErrorKind::Schema,
            error,
        )
    })?;
    drop_fulltext_table(connection, &table, &index.name, "schema analyzer probe")
}

fn ensure_fulltext_cache(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
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
        &table,
        &tokenizer,
        slot == FullTextCacheSlot::Missing,
        is_interrupted,
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
) -> QueryResult<()> {
    let analyzer = fulltext_configuration(index)?;
    build_fulltext_table(
        connection,
        snapshot,
        index,
        table,
        analyzer,
        create_table,
        is_interrupted,
    )
}

fn build_fulltext_table(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    table: &str,
    analyzer: &str,
    create_table: bool,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
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

fn drop_fulltext_table(
    connection: &Connection,
    table: &str,
    index_name: &str,
    stage: &str,
) -> QueryResult<()> {
    connection
        .execute_batch(&format!("DROP TABLE IF EXISTS temp.{table}"))
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
    let kind = match sqlite_code {
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
        |entity| {
            insert_fulltext_values(
                connection,
                &sql,
                &index.name,
                entity_id(entity),
                properties,
                |property| semantic_property(snapshot, entity, property),
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
    mut property_value: impl FnMut(&str) -> QueryResult<Value>,
) -> QueryResult<()> {
    let mut values = Vec::with_capacity(properties.len() + 1);
    let mut indexed = false;
    for property in properties {
        let text = fulltext_text(property_value(property)?);
        indexed |= text.is_some();
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

fn fulltext_text(value: Value) -> Option<String> {
    match value {
        Value::String(value) => Some(value),
        Value::List(values) => {
            let strings = values
                .into_iter()
                .map(|value| match value {
                    Value::String(value) => Some(value),
                    _ => None,
                })
                .collect::<Option<Vec<_>>>()?;
            Some(strings.join(" "))
        }
        _ => None,
    }
}

fn translate_fulltext_query(index: &IndexDefinition, query: &str) -> String {
    let Ok(properties) = index_properties(index) else {
        return query.to_owned();
    };
    let query = translate_lucene_flat_boolean_query(query);
    properties
        .iter()
        .enumerate()
        .fold(query, |query, (ordinal, property)| {
            query.replace(&format!("{property}:"), &format!("c{ordinal}:"))
        })
}

#[derive(Clone, Copy)]
enum LuceneClauseMode {
    Optional,
    Required,
    Prohibited,
}

fn translate_lucene_flat_boolean_query(query: &str) -> String {
    let Some(tokens) = split_lucene_flat_query(query) else {
        return query.to_owned();
    };
    if tokens.iter().any(|token| {
        matches!(
            token.to_ascii_uppercase().as_str(),
            "AND" | "OR" | "NOT" | "&&" | "||" | "!"
        )
    }) {
        return query.to_owned();
    }
    let clauses = tokens
        .into_iter()
        .map(lucene_clause)
        .collect::<Option<Vec<_>>>();
    let Some(clauses) = clauses else {
        return query.to_owned();
    };
    if clauses.len() <= 1 {
        return clauses
            .into_iter()
            .next()
            .map(|(_, clause)| clause)
            .unwrap_or_default();
    }
    render_lucene_flat_clauses(&clauses)
}

fn split_lucene_flat_query(query: &str) -> Option<Vec<String>> {
    let mut tokens = Vec::new();
    let mut token = String::new();
    let mut quoted = false;
    let mut escaped = false;
    for character in query.chars() {
        if escaped {
            token.push(character);
            escaped = false;
            continue;
        }
        if character == '\\' {
            token.push(character);
            escaped = true;
            continue;
        }
        if character == '"' {
            token.push(character);
            quoted = !quoted;
            continue;
        }
        if !quoted && matches!(character, '(' | ')') {
            return None;
        }
        if !quoted && character.is_whitespace() {
            if !token.is_empty() {
                tokens.push(std::mem::take(&mut token));
            }
            continue;
        }
        token.push(character);
    }
    if quoted || escaped {
        return None;
    }
    if !token.is_empty() {
        tokens.push(token);
    }
    Some(tokens)
}

fn lucene_clause(token: String) -> Option<(LuceneClauseMode, String)> {
    let (mode, clause) = match token.as_bytes().first().copied() {
        Some(b'+') => (LuceneClauseMode::Required, token.get(1..)?),
        Some(b'-') => (LuceneClauseMode::Prohibited, token.get(1..)?),
        _ => (LuceneClauseMode::Optional, token.as_str()),
    };
    if clause.is_empty() {
        return None;
    }
    Some((mode, clause.to_owned()))
}

fn render_lucene_flat_clauses(clauses: &[(LuceneClauseMode, String)]) -> String {
    let required = clauses
        .iter()
        .filter(|(mode, _)| matches!(mode, LuceneClauseMode::Required))
        .map(|(_, clause)| clause.as_str())
        .collect::<Vec<_>>();
    let optional = clauses
        .iter()
        .filter(|(mode, _)| matches!(mode, LuceneClauseMode::Optional))
        .map(|(_, clause)| clause.as_str())
        .collect::<Vec<_>>();
    let prohibited = clauses
        .iter()
        .filter(|(mode, _)| matches!(mode, LuceneClauseMode::Prohibited))
        .map(|(_, clause)| clause.as_str())
        .collect::<Vec<_>>();
    let mut rendered = if required.is_empty() {
        optional.join(" OR ")
    } else {
        required.join(" AND ")
    };
    if rendered.is_empty() {
        return String::new();
    }
    for clause in prohibited {
        rendered = format!("({rendered}) NOT {clause}");
    }
    rendered
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

fn vector_numbers(value: &Value) -> QueryResult<Vec<f32>> {
    try_vector_numbers(value)?
        .ok_or_else(|| QueryError::semantic("SEARCH FOR requires VECTOR or LIST<INTEGER | FLOAT>"))
}

fn try_vector_numbers(value: &Value) -> QueryResult<Option<Vec<f32>>> {
    let values = match value {
        Value::Vector(vector) => match vector.values() {
            VectorValues::I8(values) => values.iter().map(|value| f32::from(*value)).collect(),
            VectorValues::I16(values) => values.iter().map(|value| f32::from(*value)).collect(),
            VectorValues::I32(values) => values.iter().map(|value| *value as f32).collect(),
            VectorValues::I64(values) => values.iter().map(|value| *value as f32).collect(),
            VectorValues::F32(values) => values.clone(),
            VectorValues::F64(values) => values.iter().map(|value| *value as f32).collect(),
        },
        Value::List(values) => {
            let mut vector = Vec::with_capacity(values.len());
            for value in values {
                vector.push(match value {
                    Value::Integer(value) => *value as f32,
                    Value::Float(value) => *value as f32,
                    _ => return Ok(None),
                });
            }
            vector
        }
        _ => return Ok(None),
    };
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return Ok(None);
    }
    Ok(Some(values))
}

fn validate_query_vector(vector: &[f32], similarity: &str) -> QueryResult<()> {
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return Err(QueryError::semantic(
            "SEARCH query vector must contain finite numeric values",
        ));
    }
    if similarity == "cosine" && !has_finite_nonzero_norm(vector) {
        return Err(QueryError::semantic(
            "cosine SEARCH query vector must have a finite non-zero norm",
        ));
    }
    Ok(())
}

fn stored_vector_valid(vector: &[f32], index: &IndexDefinition) -> QueryResult<bool> {
    let (dimensions, similarity) = vector_configuration(index)?;
    if dimensions.is_some_and(|dimensions| vector.len() != dimensions) {
        return Ok(false);
    }
    if vector.is_empty() || vector.iter().any(|value| !value.is_finite()) {
        return Ok(false);
    }
    if similarity == "cosine" && !has_finite_nonzero_norm(vector) {
        return Ok(false);
    }
    Ok(true)
}
