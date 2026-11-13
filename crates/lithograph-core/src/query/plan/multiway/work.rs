use super::*;

mod binary;
mod search;

#[derive(Clone, Copy)]
struct Overhead {
    output: u64,
    memory: u64,
}

pub(super) fn choose(
    step: &MatchStep,
    original: (&[join::JoinStage], Option<u64>),
    plan: MultiwayPlan,
    (statistics, estimates): (&PlannerStatistics, &join::estimates::JoinEstimates),
    context: &join::JoinPlanningContext<'_, '_, '_>,
    resident: Option<u64>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<(MultiwayPlan, u64, u64)>> {
    let partial = resident.is_some();
    let memory = match resident {
        Some(bytes) => Ok(bytes),
        None => crate::query::stream::multiway::planning_memory_bytes(
            plan.nodes.len(),
            plan.edges.len(),
        ),
    };
    let Ok(memory) = memory else {
        return Ok(None);
    };
    if context
        .control
        .resources()
        .limit()
        .is_some_and(|limit| memory > limit.saturating_sub(context.control.resources().used()))
    {
        return Ok(None);
    }
    let output = if plan.edges.len() == step.parts.len() {
        estimates.multiway_output_rows()
    } else {
        let mask = plan
            .edges
            .iter()
            .fold(0, |mask, edge| mask | (1_u128 << edge.part));
        estimates.multiway_component_output_rows(mask)
    };
    let (Some(old), Some(output)) = (original.1, output) else {
        return Ok(None);
    };
    let Some(first) = binary::order(step, &plan, original.0, context, budget)? else {
        return Ok(None);
    };
    let first_root = first[0];
    let overhead = Overhead { output, memory };
    let candidate = search::price_order(
        (step, &plan),
        &first,
        (statistics, estimates),
        context,
        overhead,
        budget,
    )?;
    let mut best = search::BestOrder::new(old);
    best.consider(first, candidate);
    if !partial {
        search::alternatives(
            &mut best,
            (step, &plan),
            (statistics, estimates),
            context,
            overhead,
            budget,
            first_root,
        )?;
    }
    Ok(best.finish(plan)?.map(|(plan, work)| (plan, work, memory)))
}

fn order(
    step: &MatchStep,
    plan: &MultiwayPlan,
    root: usize,
    estimates: &join::estimates::JoinEstimates,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Vec<usize>>> {
    let count = plan.nodes.len();
    #[cfg(test)]
    super::tests::record_stage("order", count, budget);
    if !charge(context, budget, count, count.saturating_mul(32))? {
        return Ok(None);
    }
    let mut bound = vec![false; count];
    let mut result = vec![root];
    bound[root] = true;
    while result.len() < count {
        let mut best = None;
        for node in 0..count {
            if bound[node] {
                continue;
            }
            if !charge(context, budget, plan.edges.len(), 0)? {
                return Ok(None);
            }
            if let Some(advice) = frontier(step, plan, node, &bound, estimates) {
                let score = (advice.degree, advice.reads, node);
                if best.is_none_or(|old| score < old) {
                    best = Some(score);
                }
            }
        }
        let Some((_, _, next)) = best else {
            return Ok(None);
        };
        bound[next] = true;
        result.push(next);
    }
    Ok(Some(result))
}

struct Frontier {
    degree: u64,
    reads: u64,
    intersects: bool,
}

fn frontier(
    step: &MatchStep,
    plan: &MultiwayPlan,
    node: usize,
    bound: &[bool],
    estimates: &join::estimates::JoinEstimates,
) -> Option<Frontier> {
    let mut minimum = None;
    let mut reads = 0u64;
    let mut neighbors = 0u128;
    for edge in &plan.edges {
        let Some(other) = other(*edge, node) else {
            continue;
        };
        if other == node || !bound[other] {
            continue;
        }
        neighbors |= 1_u128 << other;
        let degree = edge_degree(step, *edge, other, estimates)?;
        minimum = Some(minimum.map_or(degree, |old: u64| old.min(degree)));
        reads = reads.checked_add(degree.checked_add(1)?)?;
    }
    Some(Frontier {
        degree: minimum?,
        reads,
        intersects: neighbors.count_ones() >= 2,
    })
}

fn edge_degree(
    step: &MatchStep,
    edge: MultiwayEdge,
    from: usize,
    estimates: &join::estimates::JoinEstimates,
) -> Option<u64> {
    let incoming = step.parts[edge.part].relationship.as_ref()?.direction == Direction::Incoming;
    let physical_target = from == edge.target;
    estimates.multiway_degree(edge.part, physical_target ^ incoming)
}

fn other(edge: MultiwayEdge, node: usize) -> Option<usize> {
    if edge.source == node {
        Some(edge.target)
    } else if edge.target == node {
        Some(edge.source)
    } else {
        None
    }
}

fn candidate_work(
    step: &MatchStep,
    plan: &MultiwayPlan,
    order: &[usize],
    statistics: &PlannerStatistics,
    estimates: &join::estimates::JoinEstimates,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    overhead: Overhead,
) -> Option<u64> {
    let (mut work, mut prefixes) = root_work(&plan.nodes[order[0]].spec, statistics, context)?;
    let mut bound = vec![false; plan.nodes.len()];
    bound[order[0]] = true;
    let mut intersection = false;
    for node in order.iter().copied().skip(1) {
        let advice = frontier(step, plan, node, &bound, estimates)?;
        intersection |= advice.intersects;
        let domain = node_domain(&plan.nodes[node].spec, statistics, context)?;
        (work, prefixes) = level_work(work, prefixes, advice.degree, advice.reads, domain)?;
        bound[node] = true;
    }
    // Exact-endpoint RID enumeration and complete BindingRow delivery are real
    // work too. Do not price only the Node set or assume intersections are empty.
    intersection.then_some(work.checked_add(delivery_work(
        overhead.output,
        plan.edges.len(),
        overhead.memory,
    )?)?)
}

fn root_work(
    root: &NodeSpec,
    statistics: &PlannerStatistics,
    context: &join::JoinPlanningContext<'_, '_, '_>,
) -> Option<(u64, u64)> {
    let prefixes = node_domain(root, statistics, context)?;
    let scan = if root.id_seek.is_some() {
        1
    } else {
        root.scan_label
            .or_else(|| context.view.scan_label())
            .and_then(|label| statistics.label_count(label))
            .or_else(|| (statistics.node_count > 0).then_some(statistics.node_count))?
    };
    Some((scan.checked_mul(3)?.checked_add(1)?, prefixes))
}

fn level_work(
    work: u64,
    prefixes: u64,
    degree: u64,
    reads: u64,
    domain: u64,
) -> Option<(u64, u64)> {
    let work = work.checked_add(prefixes.checked_mul(reads.checked_mul(3)?)?)?;
    let prefixes = prefixes.checked_mul(degree.min(domain))?;
    Some((work.checked_add(prefixes.checked_mul(2)?)?, prefixes))
}

fn delivery_work(output: u64, edges: usize, memory: u64) -> Option<u64> {
    output
        .checked_mul((edges as u64).checked_mul(6)?)?
        .checked_add(memory / 4096)
}

fn node_domain(
    node: &NodeSpec,
    statistics: &PlannerStatistics,
    context: &join::JoinPlanningContext<'_, '_, '_>,
) -> Option<u64> {
    if node.id_seek.is_some() {
        return Some(1);
    }
    node.labels
        .iter()
        .chain(context.view.required_scan_labels())
        .filter_map(|label| statistics.label_count(*label))
        .min()
        .or_else(|| (statistics.node_count > 0).then_some(statistics.node_count))
}

fn reorder(mut plan: MultiwayPlan, order: &[usize]) -> QueryResult<MultiwayPlan> {
    let invalid = || {
        crate::query::QueryError::internal("multiway variable order is not a complete permutation")
    };
    if order.len() != plan.nodes.len() {
        return Err(invalid());
    }
    let mut positions = vec![0; order.len()];
    for (new, old) in order.iter().copied().enumerate() {
        *positions.get_mut(old).ok_or_else(invalid)? = new;
    }
    let mut nodes: Vec<_> = plan.nodes.into_iter().map(Some).collect();
    plan.nodes = order
        .iter()
        .map(|old| {
            nodes
                .get_mut(*old)
                .and_then(Option::take)
                .ok_or_else(invalid)
        })
        .collect::<QueryResult<_>>()?;
    for (index, edge) in plan.edges.iter_mut().enumerate() {
        edge.source = *positions.get(edge.source).ok_or_else(invalid)?;
        edge.target = *positions.get(edge.target).ok_or_else(invalid)?;
        plan.nodes
            .get_mut(edge.source.max(edge.target))
            .ok_or_else(invalid)?
            .constraints
            .push(index);
    }
    Ok(plan)
}
