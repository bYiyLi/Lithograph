//! Query-owned search workspace preflight, conversion and bounded copy helpers.

use super::*;

pub(super) fn reserve_vector_workspace(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    dimensions: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<MemoryReservation> {
    control.check()?;
    if control.resources().limit().is_none() {
        return control.reserve(0);
    }
    let scan_memory = control.reserve(allocation_bytes::<RelationshipRecord>(
        SEMANTIC_SCAN_PAGE_SIZE * 4,
    )?)?;
    let count = semantic_candidate_upper_bound(snapshot, index, control)?;
    let maximum_connections = match &index.configuration {
        Some(IndexConfiguration::Vector { hnsw_m, .. }) => usize::try_from(*hnsw_m)
            .map_err(|_| memory_error("HNSW connection count exceeds addressable memory"))?,
        Some(IndexConfiguration::Semantic { .. }) => 16,
        _ => {
            return Err(QueryError::internal(
                "VECTOR Index is missing its versioned configuration",
            ));
        }
    };
    let build = hnsw::hnsw_workspace_bytes(count, dimensions, maximum_connections, 0)?;
    let bytes = checked_add(scan_memory.bytes(), build)?;
    drop(scan_memory);
    control.reserve(bytes)
}

fn semantic_candidate_upper_bound(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<usize> {
    let need_nodes = matches!(index.target, IndexTarget::NodeProperties { .. });
    if let Some(statistics) =
        snapshot.statistics(&BTreeSet::new(), &BTreeSet::new(), need_nodes, !need_nodes)?
    {
        let count = if need_nodes {
            statistics.node_count
        } else {
            statistics.relationship_count
        };
        return usize::try_from(count)
            .map_err(|_| memory_error("semantic candidate count exceeds addressable memory"));
    }
    scan_semantic_candidate_count(snapshot, index, control)
}

fn scan_semantic_candidate_count(
    snapshot: &Snapshot<'_>,
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<usize> {
    let mut count = 0_usize;
    let mut after = 0;
    loop {
        control.check()?;
        let (length, next_after) = match index.target {
            IndexTarget::NodeProperties { .. } => {
                let page = snapshot.scan_nodes_after(after, SEMANTIC_SCAN_PAGE_SIZE)?;
                (page.items.len(), page.next_after)
            }
            IndexTarget::RelationshipProperties { .. } => {
                let page = snapshot.scan_relationships_after(after, SEMANTIC_SCAN_PAGE_SIZE)?;
                (page.items.len(), page.next_after)
            }
            _ => return Err(QueryError::internal("semantic Index has a lookup target")),
        };
        count = count
            .checked_add(length)
            .ok_or_else(|| memory_error("semantic candidate count overflow"))?;
        let Some(next) = next_after else {
            break;
        };
        after = next;
    }
    Ok(count)
}

pub(crate) fn reserve_managed_hnsw_workspace(
    entry_count: usize,
    dimensions: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<MemoryReservation> {
    control.check()?;
    if control.resources().limit().is_none() {
        return control.reserve(0);
    }
    let build = hnsw::hnsw_workspace_bytes(entry_count, dimensions, 16, 0)?;
    control.reserve(build)
}

pub(crate) fn sort_semantic_hits_controlled(
    hits: &mut [SemanticHit],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    sort_semantic_hits_checked(hits, &mut || {
        control.map_or(Ok(()), |control| control.work(1))
    })?;
    control.map_or(Ok(()), ExecutionControl::check)
}

pub(super) fn fulltext_metadata_memory(
    index: &IndexDefinition,
    query: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<MemoryReservation>> {
    let Some(control) = control else {
        return Ok(None);
    };
    control.check()?;
    // Query translation keeps token Strings, clause vectors, rewritten query
    // text and SQL metadata simultaneously. Tokens are bounded by input bytes.
    control
        .reserve(checked_add(
            checked_mul(query.len() as u64, 256)?,
            search_index_metadata_bytes(index, control)?,
        )?)
        .map(Some)
}

pub(crate) fn search_index_metadata_bytes(
    index: &IndexDefinition,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let mut bytes = index.name.len() as u64;
    if let Some(name) = &index.owning_constraint {
        bytes = checked_add(bytes, name.len() as u64)?;
    }
    for name in index
        .labels_or_types
        .iter()
        .chain(index.additional_properties.iter())
        .chain(index_properties(index)?)
    {
        control.work(1)?;
        bytes = checked_add(bytes, checked_add(name.len() as u64, 256)?)?;
    }
    let target = match &index.target {
        IndexTarget::NodeProperties { label, .. } => label,
        IndexTarget::RelationshipProperties {
            relationship_type, ..
        } => relationship_type,
        _ => return Err(QueryError::internal("semantic Index has a lookup target")),
    };
    bytes = checked_add(bytes, target.len() as u64)?;
    bytes = checked_add(
        bytes,
        configuration_metadata_bytes(index.configuration.as_ref(), control)?,
    )?;
    // Escaped JSON cache keys, SQL metadata, membership sets and temporary
    // clones coexist. Charge their conservative capacity before building them.
    checked_add(checked_mul(bytes, 16)?, 16_384)
}

fn configuration_metadata_bytes(
    configuration: Option<&IndexConfiguration>,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    match configuration {
        Some(IndexConfiguration::FullText { analyzer, .. }) => Ok(analyzer.len() as u64),
        Some(IndexConfiguration::Vector {
            similarity_function,
            quantization_type,
            default_search_expansion_factor,
            ..
        }) => checked_add(
            similarity_function.len() as u64,
            checked_add(
                quantization_type.len() as u64,
                default_search_expansion_factor.len() as u64,
            )?,
        ),
        Some(IndexConfiguration::Semantic {
            provider,
            provider_config,
            similarity_function,
            ..
        }) => checked_add(
            provider.len() as u64,
            checked_add(
                similarity_function.len() as u64,
                crate::query::resources::json_bytes_controlled(provider_config, control)?,
            )?,
        ),
        None => Ok(0),
    }
}

pub(super) fn reserve_search_slot<T>(
    values: &mut Vec<T>,
    memory: &mut Option<MemoryReservation>,
) -> QueryResult<()> {
    if values.len() != values.capacity() {
        return Ok(());
    }
    let capacity = values
        .capacity()
        .checked_mul(2)
        .ok_or_else(|| memory_error("search result capacity overflow"))?
        .max(4);
    if let Some(memory) = memory {
        memory.grow(allocation_bytes::<T>(capacity - values.capacity())?)?;
    }
    values
        .try_reserve_exact(capacity - values.len())
        .map_err(|_| memory_error("cannot allocate search result"))?;
    Ok(())
}

pub(super) fn fulltext_text_controlled(
    value: Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<String>> {
    match value {
        Value::String(value) => Ok(Some(value)),
        Value::List(values) => {
            let mut strings = Vec::with_capacity(values.len());
            let mut length = 0_usize;
            for value in values {
                if let Some(control) = control {
                    control.work(1)?;
                }
                let Value::String(value) = value else {
                    return Ok(None);
                };
                length = length
                    .checked_add(value.len())
                    .ok_or_else(|| memory_error("full-text document length overflow"))?;
                strings.push(value);
            }
            length = length
                .checked_add(strings.len().saturating_sub(1))
                .ok_or_else(|| memory_error("full-text document length overflow"))?;
            let mut output = String::with_capacity(length);
            for (index, string) in strings.iter().enumerate() {
                if index > 0 {
                    output.push(' ');
                }
                let mut offset = 0;
                while offset < string.len() {
                    let mut end = (offset + 64 * 1024).min(string.len());
                    while !string.is_char_boundary(end) {
                        end -= 1;
                    }
                    if let Some(control) = control {
                        control.bytes((end - offset) as u64)?;
                    }
                    output.push_str(&string[offset..end]);
                    offset = end;
                }
            }
            Ok(Some(output))
        }
        _ => Ok(None),
    }
}

pub(super) fn try_vector_numbers_controlled(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(Option<Vec<f32>>, Option<MemoryReservation>)> {
    let dimension = match value {
        Value::Vector(vector) => vector.dimension(),
        Value::List(values) => values.len(),
        _ => return Ok((None, None)),
    };
    let memory = control
        .map(|control| control.reserve(allocation_bytes::<f32>(dimension)?))
        .transpose()?;
    let mut output = Vec::with_capacity(dimension);
    let valid = match value {
        Value::Vector(vector) => {
            append_typed_coordinates(&mut output, vector.values(), control)?;
            true
        }
        Value::List(values) => append_list_coordinates(&mut output, values, control)?,
        _ => unreachable!("validated vector input"),
    };
    if !valid || output.is_empty() || !finite_coordinates(&output, control)? {
        return Ok((None, None));
    }
    Ok((Some(output), memory))
}

fn append_typed_coordinates(
    output: &mut Vec<f32>,
    values: &VectorValues,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    match values {
        VectorValues::I8(values) => append_coordinates(
            output,
            values.iter().map(|value| f32::from(*value)),
            control,
        ),
        VectorValues::I16(values) => append_coordinates(
            output,
            values.iter().map(|value| f32::from(*value)),
            control,
        ),
        VectorValues::I32(values) => {
            append_coordinates(output, values.iter().map(|value| *value as f32), control)
        }
        VectorValues::I64(values) => {
            append_coordinates(output, values.iter().map(|value| *value as f32), control)
        }
        VectorValues::F32(values) => append_coordinates(output, values.iter().copied(), control),
        VectorValues::F64(values) => {
            append_coordinates(output, values.iter().map(|value| *value as f32), control)
        }
    }
}

fn append_coordinates(
    output: &mut Vec<f32>,
    values: impl Iterator<Item = f32>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    for value in values {
        if let Some(control) = control {
            control.work(1)?;
        }
        output.push(value);
    }
    Ok(())
}

fn append_list_coordinates(
    output: &mut Vec<f32>,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    for value in values {
        if let Some(control) = control {
            control.work(1)?;
        }
        output.push(match value {
            Value::Integer(value) => *value as f32,
            Value::Float(value) => *value as f32,
            _ => return Ok(false),
        });
    }
    Ok(true)
}

fn finite_coordinates(values: &[f32], control: Option<&ExecutionControl<'_>>) -> QueryResult<bool> {
    for value in values {
        if let Some(control) = control {
            control.work(1)?;
        }
        if !value.is_finite() {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn validate_query_vector_controlled(
    vector: &[f32],
    similarity: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    let mut finite = true;
    for value in vector {
        if let Some(control) = control {
            control.work(1)?;
        }
        finite &= value.is_finite();
    }
    if vector.is_empty() || !finite {
        return Err(QueryError::semantic(
            "SEARCH query vector must contain finite numeric values",
        ));
    }
    if similarity == "cosine"
        && !similarity::has_finite_nonzero_norm_controlled(vector, &|| {
            control.is_some_and(|control| control.check().is_err())
        })?
    {
        return Err(QueryError::semantic(
            "cosine SEARCH query vector must have a finite non-zero norm",
        ));
    }
    Ok(())
}

pub(super) fn stored_vector_valid_controlled(
    vector: &[f32],
    index: &IndexDefinition,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    let (dimensions, similarity) = vector_configuration(index)?;
    if dimensions.is_some_and(|dimensions| vector.len() != dimensions) {
        return Ok(false);
    }
    let mut finite = true;
    for value in vector {
        if let Some(control) = control {
            control.work(1)?;
        }
        finite &= value.is_finite();
    }
    if vector.is_empty() || !finite {
        return Ok(false);
    }
    if similarity == "cosine"
        && !similarity::has_finite_nonzero_norm_controlled(vector, &|| {
            control.is_some_and(|control| control.check().is_err())
        })?
    {
        return Ok(false);
    }
    Ok(true)
}
