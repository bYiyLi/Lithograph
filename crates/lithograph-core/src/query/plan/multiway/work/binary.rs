//! Reuse the completed binary order before spending work on alternate roots.

use super::*;

pub(super) fn order(
    step: &MatchStep,
    plan: &MultiwayPlan,
    original: &[join::JoinStage],
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Vec<usize>>> {
    let count = plan.nodes.len();
    let whole = plan.edges.len() == step.parts.len();
    let edges = plan.edges.len();
    let units = if whole {
        edges.saturating_add(count)
    } else {
        edges.saturating_mul(edges).saturating_add(count)
    };
    #[cfg(test)]
    super::super::tests::record_stage("binary-order", units, budget);
    if !charge(context, budget, units, count.saturating_mul(32))? {
        return Ok(None);
    }
    let mut seen = vec![false; count];
    let mut order = Vec::with_capacity(count);
    for stage in original.iter().take(edges) {
        // A component's freshly searched order may differ from the old plan.
        // Pay the bounded subset lookup; retain direct whole-MATCH ordinals.
        let edge = if whole {
            plan.edges.get(stage.part)
        } else {
            plan.edges.iter().find(|edge| edge.part == stage.part)
        };
        let Some(edge) = edge.filter(|edge| edge.part == stage.part) else {
            return Ok(None);
        };
        let incoming = step.parts[stage.part]
            .relationship
            .as_ref()
            .is_some_and(|rel| rel.direction == Direction::Incoming);
        let target_first = (stage.orientation == join::PartOrientation::Reversed) ^ incoming;
        let (first, second) = if target_first {
            (edge.target, edge.source)
        } else {
            (edge.source, edge.target)
        };
        append(first, &mut seen, &mut order);
        append(second, &mut seen, &mut order);
    }
    Ok((order.len() == count).then_some(order))
}

fn append(node: usize, seen: &mut [bool], order: &mut Vec<usize>) {
    if !seen[node] {
        seen[node] = true;
        order.push(node);
    }
}
