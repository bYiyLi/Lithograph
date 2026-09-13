use std::collections::BTreeSet;

use rusqlite::{Connection, OptionalExtension as _, params_from_iter};

use crate::cypher::{Value, VectorValues};
use crate::storage::{
    self, IndexConfiguration, IndexDefinition, IndexTarget, RelationshipRecord, Snapshot,
    StandardIndexKind,
};

use super::graph::{self, ResolvedGraphView};
use super::{QueryError, QueryResult};

mod hnsw;
use hnsw::{build_vector_cache, query_vector_cache};

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
    for entity in indexed_entities(
        connection,
        snapshot,
        index,
        Some(graph_view),
        is_interrupted,
    )? {
        let value = semantic_property(snapshot, entity, property)?;
        if let Some(hit) = vector_hit(index, input, entity, value)? {
            hits.push(hit);
        }
    }
    Ok(hits)
}

fn sort_semantic_hits(hits: &mut [SemanticHit]) {
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

fn indexed_entities(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    graph_view: Option<&ResolvedGraphView>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<SemanticEntity>> {
    let membership = semantic_membership(connection, index)?;
    let candidates = snapshot_semantic_entities(snapshot, &membership)?;
    let mut entities = Vec::new();
    for entity in candidates {
        if is_interrupted() {
            return Err(QueryError::interrupted());
        }
        if !semantic_entity_matches(snapshot, entity, &membership)? {
            continue;
        }
        if let Some(view) = graph_view
            && !semantic_entity_visible(snapshot, view, entity)?
        {
            continue;
        }
        entities.push(entity);
    }
    Ok(entities)
}

enum SemanticMembership {
    NodeLabels(BTreeSet<i64>),
    RelationshipTypes(BTreeSet<i64>),
}

fn semantic_membership(
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

fn snapshot_semantic_entities(
    snapshot: &Snapshot<'_>,
    membership: &SemanticMembership,
) -> QueryResult<Vec<SemanticEntity>> {
    let mut entities = Vec::new();
    match membership {
        SemanticMembership::NodeLabels(_) => snapshot.visit_nodes(|node| {
            entities.push(SemanticEntity::Node(node));
            Ok(())
        })?,
        SemanticMembership::RelationshipTypes(_) => {
            snapshot.visit_relationships(|relationship| {
                entities.push(SemanticEntity::Relationship(relationship));
                Ok(())
            })?
        }
    }
    Ok(entities)
}

fn semantic_entity_matches(
    snapshot: &Snapshot<'_>,
    entity: SemanticEntity,
    membership: &SemanticMembership,
) -> QueryResult<bool> {
    match (entity, membership) {
        (SemanticEntity::Node(node), SemanticMembership::NodeLabels(labels)) => {
            node_has_any_label(snapshot, node, labels)
        }
        (
            SemanticEntity::Relationship(relationship),
            SemanticMembership::RelationshipTypes(types),
        ) => Ok(types.contains(&relationship.type_id)),
        _ => Ok(false),
    }
}

fn semantic_entity_visible(
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

fn semantic_property(
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

fn node_has_any_label(
    snapshot: &Snapshot<'_>,
    node: i64,
    labels: &BTreeSet<i64>,
) -> QueryResult<bool> {
    if labels.is_empty() {
        return Ok(false);
    }
    Ok(snapshot
        .labels(node)?
        .into_iter()
        .any(|label| labels.contains(&label)))
}

fn entity_id(entity: SemanticEntity) -> i64 {
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
    let table = ensure_fulltext_cache(connection, snapshot, index, is_interrupted)?;
    if let Some(analyzer) = input.analyzer
        && analyzer != configured_analyzer
    {
        return fulltext_query_with_analyzer_override(
            connection,
            snapshot,
            graph_view,
            index,
            input,
            &table,
            is_interrupted,
        );
    }
    let candidates = default_fulltext_candidates(connection, index, input.query, &table)?;
    visible_fulltext_hits(snapshot, graph_view, input, candidates, is_interrupted)
}

fn fulltext_query_with_analyzer_override(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &FullTextQueryInput<'_>,
    table: &str,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<SemanticHit>> {
    let analyzer = input
        .analyzer
        .ok_or_else(|| QueryError::internal("full-text analyzer override is missing"))?;
    let terms = analyze_fulltext_query(connection, input.query, analyzer)?;
    if terms.is_empty() {
        return Ok(Vec::new());
    }
    let candidates = override_fulltext_candidates(connection, table, &terms)?;
    let _ = index;
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
    let mut statement = connection.prepare(&sql)?;
    statement
        .query_map([translated], |row| {
            let raw_score = row.get::<_, f64>(1)?;
            Ok(FullTextCandidate {
                owner_id: row.get(0)?,
                score: normalize_fulltext_score(raw_score),
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
}

fn override_fulltext_candidates(
    connection: &Connection,
    table: &str,
    terms: &[String],
) -> QueryResult<Vec<FullTextCandidate>> {
    let vocab = ensure_fulltext_vocab(connection, table)?;
    let placeholders = (1..=terms.len())
        .map(|index| format!("?{index}"))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT f.owner_id, count(DISTINCT v.term) AS matched \
         FROM temp.{vocab} v JOIN temp.{table} f ON f.rowid = v.doc \
         WHERE v.term IN ({placeholders}) GROUP BY f.owner_id \
         ORDER BY matched DESC, f.owner_id"
    );
    let parameters = terms
        .iter()
        .map(|term| rusqlite::types::Value::Text(term.clone()))
        .collect::<Vec<_>>();
    let mut statement = connection.prepare(&sql)?;
    statement
        .query_map(params_from_iter(parameters), |row| {
            Ok(FullTextCandidate {
                owner_id: row.get(0)?,
                score: row.get::<_, i64>(1)? as f64 / terms.len() as f64,
            })
        })?
        .collect::<Result<Vec<_>, _>>()
        .map_err(Into::into)
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

fn ensure_fulltext_vocab(connection: &Connection, table: &str) -> QueryResult<String> {
    let vocab = format!("{table}_vocab");
    connection.execute_batch(&format!(
        "CREATE VIRTUAL TABLE IF NOT EXISTS temp.{vocab} USING fts5vocab(temp, {table}, instance)"
    ))?;
    Ok(vocab)
}

fn analyze_fulltext_query(
    connection: &Connection,
    query: &str,
    analyzer: &str,
) -> QueryResult<Vec<String>> {
    let tokenizer = fulltext_tokenizer(analyzer)?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_FTS5_QUERY_ANALYZER_V1");
    hasher.update(analyzer.as_bytes());
    hasher.update(query.as_bytes());
    let digest = hasher.finalize().to_hex().to_string();
    let table = format!("_lithograph_fts_query_{}", &digest[..20]);
    let vocab = format!("{table}_vocab");
    connection.execute_batch(&format!(
        "DROP TABLE IF EXISTS temp.{vocab}; \
         DROP TABLE IF EXISTS temp.{table}; \
         CREATE VIRTUAL TABLE temp.{table} USING fts5(value, tokenize='{tokenizer}'); \
         CREATE VIRTUAL TABLE temp.{vocab} USING fts5vocab(temp, {table}, instance);"
    ))?;
    connection.execute(
        &format!("INSERT INTO temp.{table}(value) VALUES(?1)"),
        [query],
    )?;
    let mut statement = connection.prepare(&format!(
        "SELECT DISTINCT term FROM temp.{vocab} ORDER BY term"
    ))?;
    let terms = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    connection.execute_batch(&format!(
        "DROP TABLE IF EXISTS temp.{vocab}; DROP TABLE IF EXISTS temp.{table};"
    ))?;
    Ok(terms)
}

fn fulltext_tokenizer(analyzer: &str) -> QueryResult<&'static str> {
    match analyzer {
        "standard-no-stop-words" => Ok("unicode61"),
        "english" => Ok("porter unicode61"),
        other => Err(QueryError::semantic(format!(
            "unsupported full-text analyzer {other:?}"
        ))),
    }
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

fn ensure_fulltext_cache(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<String> {
    let table = fulltext_cache_table(snapshot, index)?;
    let exists = connection
        .query_row(
            "SELECT 1 FROM temp.sqlite_schema WHERE type = 'table' AND name = ?1",
            [&table],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .is_some();
    if exists {
        return Ok(table);
    }
    build_fulltext_cache(connection, snapshot, index, &table, is_interrupted)?;
    Ok(table)
}

fn fulltext_cache_table(snapshot: &Snapshot<'_>, index: &IndexDefinition) -> QueryResult<String> {
    let digest = semantic_cache_digest(snapshot, index, b"LITHOGRAPH_FTS5_CACHE_V1")?;
    Ok(format!("_lithograph_fts_{}", &digest[..24]))
}

fn build_fulltext_cache(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    table: &str,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let properties = index_properties(index)?;
    let analyzer = fulltext_configuration(index)?;
    let tokenizer = fulltext_tokenizer(analyzer)?;
    let columns = (0..properties.len())
        .map(|index| format!("c{index}"))
        .collect::<Vec<_>>();
    let create = format!(
        "CREATE VIRTUAL TABLE temp.{table} USING fts5({}, owner_id UNINDEXED, tokenize='{tokenizer}')",
        columns.join(", ")
    );
    connection.execute_batch(&create)?;
    let result = populate_fulltext_cache(
        connection,
        snapshot,
        index,
        table,
        properties,
        is_interrupted,
    );
    if result.is_err() {
        let _ = connection.execute_batch(&format!("DROP TABLE IF EXISTS temp.{table}"));
    }
    result
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
    for entity in indexed_entities(connection, snapshot, index, None, is_interrupted)? {
        insert_fulltext_values(
            connection,
            &sql,
            entity_id(entity),
            properties,
            |property| semantic_property(snapshot, entity, property),
        )?;
    }
    Ok(())
}

fn insert_fulltext_values(
    connection: &Connection,
    sql: &str,
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
        connection.execute(sql, params_from_iter(values))?;
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

fn semantic_cache_digest(
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
    if similarity == "cosine" {
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm == 0.0 || !norm.is_finite() {
            return Err(QueryError::semantic(
                "cosine SEARCH query vector must have a finite non-zero norm",
            ));
        }
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
    if similarity == "cosine" {
        let norm = vector.iter().map(|value| value * value).sum::<f32>().sqrt();
        if norm == 0.0 || !norm.is_finite() {
            return Ok(false);
        }
    }
    Ok(true)
}

fn vector_similarity_numbers(left: &[f32], right: &[f32], similarity: &str) -> QueryResult<f64> {
    if left.len() != right.len() {
        return Err(QueryError::semantic("vector dimensions must match"));
    }
    match similarity {
        "cosine" => {
            let dot = left.iter().zip(right).map(|(a, b)| a * b).sum::<f32>();
            let left_norm = left.iter().map(|value| value * value).sum::<f32>().sqrt();
            let right_norm = right.iter().map(|value| value * value).sum::<f32>().sqrt();
            if left_norm == 0.0 || right_norm == 0.0 {
                return Err(QueryError::semantic(
                    "cosine similarity requires non-zero vectors",
                ));
            }
            Ok(f64::from(
                ((1.0 + dot / (left_norm * right_norm)) / 2.0).clamp(0.0, 1.0),
            ))
        }
        "euclidean" => {
            let distance = left
                .iter()
                .zip(right)
                .map(|(a, b)| {
                    let delta = a - b;
                    delta * delta
                })
                .sum::<f32>();
            Ok(f64::from(1.0 / (1.0 + distance)))
        }
        other => Err(QueryError::internal(format!(
            "unsupported persisted vector similarity {other:?}"
        ))),
    }
}
