use super::*;

mod component;
pub(crate) use component::describe_component;

pub(super) fn describe(
    step: &MatchStep,
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<MultiwayPlan>> {
    if step.optional
        || step.parts.len() < 3
        || scope.snapshot_identity != context.snapshot.cache_identity()
        || !step.parts.iter().all(supported)
    {
        return Ok(None);
    }
    let bytes = metadata::description_bytes(step);
    if !charge(context, budget, step.parts.len().saturating_mul(8), bytes)? {
        return Ok(None);
    }
    let mut plan = MultiwayPlan {
        clause_start: scope.clause_start,
        snapshot_identity: scope.snapshot_identity,
        nodes: Vec::new(),
        edges: Vec::with_capacity(step.parts.len()),
    };
    for (part, pattern) in step.parts.iter().enumerate() {
        if !append(&mut plan, part, pattern, context, budget)? {
            return Ok(None);
        }
    }
    if plan.nodes.len() < 3 || plan.edges.len() < plan.nodes.len() {
        return Ok(None);
    }
    for node in &mut plan.nodes {
        select_label(&mut node.spec, statistics);
    }
    Ok(Some(plan))
}

fn supported(pattern: &PatternPart) -> bool {
    let (Some(relationship), Some(end)) = (&pattern.relationship, &pattern.end) else {
        return false;
    };
    relationship.type_id.is_some_and(|id| id > 0)
        && pattern.path_variable.is_none()
        && relationship.direction != Direction::Undirected
        && relationship.id_seek.is_none()
        && relationship.index_seek.is_none()
        && !relationship.impossible
        && plain(&pattern.start)
        && plain(end)
}

fn append(
    plan: &mut MultiwayPlan,
    part: usize,
    pattern: &PatternPart,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    let (Some(relationship), Some(end)) = (&pattern.relationship, &pattern.end) else {
        return Ok(false);
    };
    let Some(type_id) = relationship.type_id.filter(|id| *id > 0) else {
        return Ok(false);
    };
    let Some(start) = intern(plan, &pattern.start, context, budget)? else {
        return Ok(false);
    };
    let Some(end) = intern(plan, end, context, budget)? else {
        return Ok(false);
    };
    let (source, target) = if relationship.direction == Direction::Outgoing {
        (start, end)
    } else {
        (end, start)
    };
    plan.edges.push(MultiwayEdge {
        part,
        source,
        target,
        type_id,
    });
    Ok(true)
}

fn plain(node: &NodeSpec) -> bool {
    node.variable.as_ref().is_some_and(|name| !name.is_empty())
        && node.index_seek.is_none()
        && !node.impossible
        && node.labels.len() == node.label_names.len()
}

fn intern(
    plan: &mut MultiwayPlan,
    spec: &NodeSpec,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<usize>> {
    let name = spec.variable.as_deref().unwrap_or_default();
    let work = plan
        .nodes
        .len()
        .saturating_add(spec.labels.len().saturating_mul(2));
    let comparisons = plan.nodes.iter().fold(0usize, |bytes, node| {
        bytes.saturating_add(name.len().min(node.variable.len()))
    });
    if !charge(context, budget, work, comparisons)? {
        return Ok(None);
    }
    if let Some(index) = plan.nodes.iter().position(|node| node.variable == name) {
        let node = &mut plan.nodes[index].spec;
        if node.id_seek.zip(spec.id_seek).is_some_and(|(a, b)| a != b) {
            return Ok(None);
        }
        node.id_seek = node.id_seek.or(spec.id_seek);
        for (id, name) in spec.labels.iter().zip(&spec.label_names) {
            if !charge(context, budget, node.labels.len().saturating_add(1), 0)? {
                return Ok(None);
            }
            if !node.labels.contains(id) {
                node.labels.push(*id);
                node.label_names.push(name.clone());
            }
        }
        return Ok(Some(index));
    }
    let index = plan.nodes.len();
    plan.nodes.push(MultiwayNode {
        variable: name.to_owned(),
        spec: spec.clone(),
        constraints: Vec::new(),
    });
    Ok(Some(index))
}

fn select_label(node: &mut NodeSpec, statistics: &PlannerStatistics) {
    if let Some((index, _)) = node
        .labels
        .iter()
        .enumerate()
        .filter_map(|(index, id)| statistics.label_count(*id).map(|count| (index, count)))
        .min_by_key(|(_, count)| *count)
    {
        node.scan_label = Some(node.labels[index]);
        node.scan_label_name = Some(node.label_names[index].clone());
    }
}
