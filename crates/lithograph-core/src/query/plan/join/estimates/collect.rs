use super::*;
use crate::query::resources::MemoryReservation;
use crate::query::schema::{IndexDistribution, StandardIndexSeek};

pub(super) fn claim(
    work: usize,
    budget: &mut PlanningBudget,
    headroom: usize,
    context: &JoinPlanningContext<'_, '_, '_>,
) -> QueryResult<Option<MemoryReservation>> {
    let bytes = 16 * 1024;
    // Optional advice leaves enough of the shared ledger for a complete Greedy.
    // Refusing a sample does not exhaust the search or execution budget.
    if work.saturating_add(headroom) > budget.remaining_states()
        || bytes > budget.remaining_advice_bytes()
    {
        return Ok(None);
    }
    let memory = match context.control.reserve(bytes as u64) {
        Ok(memory) => memory,
        Err(error) if error.kind == QueryErrorKind::Resource => return Ok(None),
        Err(error) => return Err(error),
    };
    Ok(budget.work(work, bytes).then_some(memory))
}

pub(super) fn distribution(
    name: &str,
    definition: crate::storage::HashId,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<Option<IndexDistribution>> {
    let snapshot = context.snapshot.cache_identity();
    if let Some(cached) = budget.cached_join_distribution(snapshot, definition) {
        return Ok(cached);
    }
    let Some(index) = context.schema.indexes.get(name) else {
        return Ok(None);
    };
    if index.kind != crate::storage::StandardIndexKind::Range {
        return Ok(None);
    }
    let Some(_memory) = claim(1024, budget, headroom, context)? else {
        return Ok(None);
    };
    let sample = crate::query::schema::estimate_index_distribution(
        context.snapshot,
        index,
        &context.view.query_access,
        context.control,
    )?;
    let _ = budget.remember_join_distribution(snapshot, definition, sample);
    Ok(sample)
}

pub(super) fn existing_index(
    seek: &StandardIndexSeek,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<Option<CardinalityEstimate>> {
    let Some(index) = context.schema.indexes.get(&seek.index_name) else {
        return Ok(None);
    };
    if !matches!(
        seek.predicates.as_slice(),
        [(0, crate::query::schema::StandardIndexPredicate::Equal(_))]
    ) {
        return Ok(None);
    }
    if index.kind != crate::storage::StandardIndexKind::Range {
        return Ok(None);
    }
    let Some(_memory) = claim(1024, budget, headroom, context)? else {
        return Ok(None);
    };
    Ok(crate::query::schema::estimate_index_access(
        context.snapshot,
        index,
        seek,
        &context.view.query_access,
        context.control,
    )?
    .map(|estimate| estimate.rows))
}

pub(super) fn sample_degree(
    part: &PatternPart,
    target: ProbeTarget,
    domain: Option<(Option<crate::storage::LabelId>, u64)>,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<Option<CardinalityEstimate>> {
    let (Some(rel), Some(domain)) = (&part.relationship, domain) else {
        return Ok(None);
    };
    // Broad cohorts use already collected type totals. Probe small cohorts to
    // observe skew, rather than repeating an ANALYZE for every candidate.
    if domain.1 > 32 || target == ProbeTarget::Relationship {
        return Ok(None);
    }
    let Some(_memory) = claim(512, budget, headroom, context)? else {
        return Ok(None);
    };
    let reversed = target == ProbeTarget::EndNode;
    let incoming = match (rel.direction, reversed) {
        (Direction::Outgoing, false) | (Direction::Incoming, true) => Some(false),
        (Direction::Incoming, false) | (Direction::Outgoing, true) => Some(true),
        (Direction::Undirected, _) => None,
    };
    let check = || {
        context
            .control
            .check()
            .map_err(crate::query::QueryError::into_storage_error)
    };
    let sample = context.snapshot.bounded_degree_statistics(
        domain,
        rel.type_id,
        incoming,
        16,
        512,
        16 * 1024,
        &check,
    )?;
    Ok(sample
        .filter(|sample| sample.sample_rows > 0 && sample.work <= 512 && sample.bytes <= 16 * 1024)
        .map(|sample| {
            expectation(
                sample.expected.div_ceil(domain.1.max(1)),
                // A complete cohort's mean is not a bound for one hot outer key.
                if sample.exhaustive && sample.truncated_nodes == 0 {
                    sample.max_observed
                } else {
                    u64::MAX
                },
            )
        }))
}
