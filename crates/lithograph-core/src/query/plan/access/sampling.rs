//! Optional samples share the caller's single planning and memory budgets.
use super::*;

pub(super) fn claim(
    work: usize,
    bytes: usize,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<MemoryReservation>> {
    if budget.exhausted()
        || work > budget.remaining_states()
        || bytes > budget.remaining_advice_bytes()
    {
        return Ok(None);
    }
    let memory = match control.reserve(bytes as u64) {
        Ok(memory) => memory,
        Err(error) if error.kind == crate::query::QueryErrorKind::Resource => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(budget.work(work, bytes).then_some(memory))
}

pub(super) fn combination(
    node: &NodeSpec,
    view: &ResolvedGraphView,
    snapshot: &storage::Snapshot<'_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<crate::query::stats::CardinalityEstimate>> {
    let slots = node
        .labels
        .len()
        .saturating_add(view.required_scan_labels().len());
    if !(2..=32).contains(&slots) {
        return Ok(None);
    }
    let Some(_memory) = claim(256, 4096, budget, control)? else {
        return Ok(None);
    };
    let labels = node
        .labels
        .iter()
        .chain(view.required_scan_labels())
        .copied()
        .collect::<BTreeSet<_>>();
    if labels.len() < 2 {
        return Ok(None);
    }
    let Some(domain) = labels
        .iter()
        .filter_map(|id| statistics.label_count(*id).map(|count| (*id, count)))
        .min_by_key(|entry| entry.1)
    else {
        return Ok(None);
    };
    let check = || control.check().map_err(QueryError::into_storage_error);
    let Some(sample) =
        snapshot.bounded_label_combination(domain, &labels, 16, 256, 4096, &check)?
    else {
        return Ok(None);
    };
    if sample.work > 256
        || sample.bytes > 4096
        || sample.sample_matches > sample.sample_rows
        || (!sample.exhaustive && sample.sample_rows < 8)
    {
        return Ok(None);
    }
    Ok(Some(crate::query::stats::CardinalityEstimate {
        expected: sample.expected,
        lower: sample.lower,
        upper: sample.upper,
    }))
}

pub(super) fn prefer_adjacency(
    part: &PatternPart,
    view: &ResolvedGraphView,
    snapshot: &storage::Snapshot<'_>,
    schema: &storage::SchemaState,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let Some(rel) = part.relationship.as_ref() else {
        return Ok(false);
    };
    if rel.id_seek.is_some() || part.start.id_seek.is_some() || part.start.index_seek.is_some() {
        return Ok(false);
    }
    let indexed = indexed_rows(rel, view, snapshot, schema, statistics, budget, control)?;
    let Some(indexed) = indexed else {
        return Ok(false);
    };
    let label = part.start.scan_label.or_else(|| view.scan_label());
    let count = match label {
        Some(id) => statistics.label_count(id),
        None => Some(statistics.node_count),
    };
    let Some(count) = count else { return Ok(false) };
    let Some(_memory) = claim(512, 16 * 1024, budget, control)? else {
        return Ok(false);
    };
    let incoming = match rel.direction {
        Direction::Outgoing => Some(false),
        Direction::Incoming => Some(true),
        Direction::Undirected => None,
    };
    let check = || control.check().map_err(QueryError::into_storage_error);
    let Some(sample) = snapshot.bounded_degree_statistics(
        (label, count),
        rel.type_id,
        incoming,
        16,
        512,
        16 * 1024,
        &check,
    )?
    else {
        return Ok(false);
    };
    let valid = [
        sample.work <= 512,
        sample.bytes <= 16 * 1024,
        sample.sample_rows > 0,
        sample.buckets.iter().sum::<u64>() == sample.sample_rows as u64,
        sample.truncated_nodes == 0,
        sample.exhaustive || sample.sample_rows >= 8,
        sample.mean.is_finite(),
    ];
    if !valid.into_iter().all(|valid| valid) {
        return Ok(false);
    }
    let estimated = (sample.mean * count as f64).ceil() as u64;
    let expanded = sample.upper.unwrap_or_else(|| {
        sample
            .expected
            .max(estimated)
            .max(sample.lower)
            .max(sample.max_observed)
    });
    let skew = sample.buckets[4].saturating_mul(expanded) / sample.sample_rows as u64;
    let cost = count
        .saturating_mul(2)
        .saturating_add(expanded.saturating_mul(3))
        .saturating_add(skew);
    // A small established index probe remains preferable without a material
    // reduction. Startup guesses alone must not switch it to a node scan.
    Ok(cost < indexed.saturating_mul(2))
}

fn indexed_rows(
    rel: &RelationshipSpec,
    view: &ResolvedGraphView,
    snapshot: &storage::Snapshot<'_>,
    schema: &storage::SchemaState,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<u64>> {
    let Some(seek) = rel.index_seek.as_ref() else {
        return Ok(None);
    };
    if seek.kind == StandardIndexKind::Lookup {
        return Ok(rel
            .type_id
            .and_then(|id| statistics.type_counts.get(&id).copied()));
    }
    if !matches!(
        seek.predicates.as_slice(),
        [(0, StandardIndexPredicate::Equal(_))]
    ) {
        return Ok(None);
    }
    let Some(index) = schema.indexes.get(&seek.index_name) else {
        return Ok(None);
    };
    if !matches!(&index.target, storage::IndexTarget::RelationshipProperties { properties, .. } if properties.len() == 1)
    {
        return Ok(None);
    }
    let Some(_memory) = claim(1024, 16 * 1024, budget, control)? else {
        return Ok(None);
    };
    Ok(super::super::super::schema::estimate_index_access(
        snapshot,
        index,
        seek,
        &view.query_access,
        control,
    )?
    .map(|estimate| estimate.rows.expected))
}
