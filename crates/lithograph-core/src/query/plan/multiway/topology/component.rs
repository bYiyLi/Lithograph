//! Find one supported connected cycle without borrowing the old binary order.

use super::*;

pub(crate) fn describe_component(
    step: &MatchStep,
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<MultiwayPlan>> {
    // Prefix resident sizing currently proves a fresh base. Unknown incoming
    // values keep the already completed binary plan instead of inventing size.
    if step.optional
        || step.parts.len() < 4
        || !scope.complete_consumer
        || !scope.incoming.is_empty()
        || scope.snapshot_identity != context.snapshot.cache_identity()
    {
        return Ok(None);
    }
    // A complete supported MATCH uses its existing whole-plan candidate.
    // This bounded borrowed shape check allocates and reads no graph data.
    let Some(count) = component_edge_count(step) else {
        return Ok(None);
    };
    if !charge(
        context,
        budget,
        step.parts.len().saturating_mul(8),
        metadata::description_bytes(step),
    )? {
        return Ok(None);
    }
    let mut plan = MultiwayPlan {
        clause_start: scope.clause_start,
        snapshot_identity: scope.snapshot_identity,
        nodes: Vec::new(),
        edges: Vec::with_capacity(count),
    };
    for (part, pattern) in step.parts.iter().enumerate() {
        if supported(pattern) && !append(&mut plan, part, pattern, context, budget)? {
            return Ok(None);
        }
    }
    let Some(mut plan) = select(plan, context, budget)? else {
        return Ok(None);
    };
    for node in &mut plan.nodes {
        select_label(&mut node.spec, statistics);
    }
    Ok(Some(plan))
}

fn component_edge_count(step: &MatchStep) -> Option<usize> {
    let count = step.parts.iter().filter(|part| supported(part)).count();
    (count >= 3 && count < step.parts.len()).then_some(count)
}

fn select(
    mut plan: MultiwayPlan,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<MultiwayPlan>> {
    if plan.nodes.len() < 3 || plan.nodes.len() > 128 {
        return Ok(None);
    }
    let mut remaining = if plan.nodes.len() == 128 {
        u128::MAX
    } else {
        (1_u128 << plan.nodes.len()) - 1
    };
    while remaining != 0 {
        let Some(nodes) = connected(&plan, remaining.trailing_zeros(), context, budget)? else {
            return Ok(None);
        };
        remaining &= !nodes;
        if !charge(context, budget, plan.edges.len(), 0)? {
            return Ok(None);
        }
        let edges = plan
            .edges
            .iter()
            .filter(|edge| nodes & (1_u128 << edge.source) != 0)
            .count();
        if nodes.count_ones() < 3 || edges < nodes.count_ones() as usize {
            continue;
        }
        if !restrict(&mut plan, nodes, context, budget)? {
            return Ok(None);
        }
        return Ok(Some(plan));
    }
    Ok(None)
}

fn connected(
    plan: &MultiwayPlan,
    root: u32,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<u128>> {
    let mut nodes = 1_u128 << root;
    loop {
        let previous = nodes;
        if !charge(context, budget, plan.edges.len(), 0)? {
            return Ok(None);
        }
        for edge in &plan.edges {
            let endpoints = (1_u128 << edge.source) | (1_u128 << edge.target);
            if nodes & endpoints != 0 {
                nodes |= endpoints;
            }
        }
        if previous == nodes {
            return Ok(Some(nodes));
        }
    }
}

fn restrict(
    plan: &mut MultiwayPlan,
    nodes: u128,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    let count = plan.nodes.len();
    if !charge(
        context,
        budget,
        count.saturating_mul(2).saturating_add(plan.edges.len()),
        count.saturating_mul(std::mem::size_of::<usize>()),
    )? {
        return Ok(false);
    }
    let mut positions = vec![0usize; count];
    let mut next = 0;
    for (old, position) in positions.iter_mut().enumerate() {
        if nodes & (1_u128 << old) != 0 {
            *position = next;
            next += 1;
        }
    }
    let mut old = 0;
    plan.nodes.retain(|_| {
        let keep = nodes & (1_u128 << old) != 0;
        old += 1;
        keep
    });
    plan.edges.retain_mut(|edge| {
        if nodes & (1_u128 << edge.source) == 0 {
            return false;
        }
        edge.source = positions[edge.source];
        edge.target = positions[edge.target];
        true
    });
    Ok(true)
}
