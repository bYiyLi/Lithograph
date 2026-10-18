//! Vector search paths share the caller execution budget.

use super::*;

pub(crate) fn vector_candidates_controlled(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    query: &Value,
    candidate_limit: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<VectorCandidateResult> {
    let is_interrupted = || control.check().is_err();
    let _metadata_memory = control.reserve(search_index_metadata_bytes(index, control)?)?;
    validate_query_index_access(index, graph_view)?;
    let (query_vector, _query_memory) = try_vector_numbers_controlled(query, Some(control))?;
    let query_vector = query_vector.ok_or_else(|| {
        QueryError::semantic("SEARCH FOR requires VECTOR or LIST<INTEGER | FLOAT>")
    })?;
    let similarity = validate_vector_search_input_controlled(index, &query_vector, Some(control))?;
    if let Some(result) = cached_vector_candidates(
        snapshot,
        graph_view,
        index,
        query,
        candidate_limit,
        &is_interrupted,
        control,
    )? {
        return Ok(result);
    }
    // Cold-cache fallback and construction can retain all indexed owners even
    // for LIMIT 1. Warm queries reserve only the requested cache, without a scan.
    let workspace = reserve_vector_workspace(snapshot, index, query_vector.len(), control)?;
    let input = VectorSearchInput {
        query: &query_vector,
        similarity,
    };
    let mut hits = scan_vector_candidates(
        connection,
        snapshot,
        graph_view,
        index,
        &input,
        &is_interrupted,
        Some(control),
    )?;
    sort_semantic_hits_controlled(&mut hits, Some(control))?;
    hnsw::build_vector_cache_controlled(
        connection,
        snapshot,
        index,
        query_vector.len(),
        &is_interrupted,
        Some(control),
    )?;
    retain_candidate_memory(
        VectorCandidateResult {
            hits,
            exhaustive: true,
            memory: None,
        },
        workspace,
    )
}

fn cached_vector_candidates(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    query: &Value,
    candidate_limit: usize,
    is_interrupted: &dyn Fn() -> bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<VectorCandidateResult>> {
    let Some(mut result) = hnsw::query_vector_cache_controlled(
        snapshot,
        graph_view,
        index,
        query,
        candidate_limit,
        is_interrupted,
        Some(control),
    )?
    else {
        return Ok(None);
    };
    let _sorting = control.reserve(allocation_bytes::<SemanticHit>(result.hits.len())?)?;
    sort_semantic_hits_controlled(&mut result.hits, Some(control))?;
    Ok(Some(result))
}

fn retain_candidate_memory(
    mut result: VectorCandidateResult,
    mut memory: MemoryReservation,
) -> QueryResult<VectorCandidateResult> {
    memory.resize(allocation_bytes::<SemanticHit>(result.hits.capacity())?)?;
    result.memory = Some(memory);
    Ok(result)
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

fn validate_vector_search_input_controlled<'a>(
    index: &'a IndexDefinition,
    query_vector: &[f32],
    control: Option<&ExecutionControl<'_>>,
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
    validate_query_vector_controlled(query_vector, similarity, control)?;
    Ok(similarity)
}

fn scan_vector_candidates(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    index: &IndexDefinition,
    input: &VectorSearchInput<'_>,
    is_interrupted: &dyn Fn() -> bool,
    control: Option<&ExecutionControl<'_>>,
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
        control,
        |entity| {
            let (value, _property_memory) =
                semantic_property_controlled(snapshot, entity, property, control)?;
            if let Some(hit) = vector_hit_controlled(index, input, entity, value, control)? {
                hits.push(hit);
            }
            Ok(())
        },
    )?;
    Ok(hits)
}
