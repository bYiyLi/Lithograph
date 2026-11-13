//! A bounded candidate for ordinary directed cyclic MATCH components.
//! Original PatternParts and the complete WHERE remain the semantic source.

use super::*;

mod metadata;
pub(crate) use metadata::headroom_bytes;
#[cfg(test)]
mod tests;
mod topology;
pub(crate) use topology::describe_component;
mod work;

#[derive(Debug, Clone)]
pub(crate) struct MultiwayPlan {
    pub(crate) clause_start: usize,
    pub(crate) snapshot_identity: HashId,
    /// Node variable order; the first node is the existing access root.
    pub(crate) nodes: Vec<MultiwayNode>,
    pub(crate) edges: Vec<MultiwayEdge>,
}

#[derive(Debug, Clone)]
pub(crate) struct MultiwayNode {
    pub(crate) variable: String,
    /// Conjunction of every occurrence's labels, with one root access.
    pub(crate) spec: NodeSpec,
    /// Edges whose two endpoints first become bound at this variable.
    pub(crate) constraints: Vec<usize>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct MultiwayEdge {
    pub(crate) part: usize,
    /// Physical directed endpoints, normalized from the written direction.
    pub(crate) source: usize,
    pub(crate) target: usize,
    pub(crate) type_id: RelationshipTypeId,
}

/// A completed specialization, priced before optional exact binary search.
pub(crate) struct PricedMultiway {
    description: MultiwayPlan,
    work: u64,
}

/// The already completed binary candidate and the work paid by its search.
#[derive(Clone, Copy)]
pub(crate) struct CompletedJoin<'a> {
    pub(crate) plan: &'a join::JoinPlan,
    pub(crate) work: Option<u64>,
}

impl PricedMultiway {
    pub(crate) fn work(&self) -> u64 {
        self.work
    }

    pub(crate) fn install(self, plan: &mut join::JoinPlan, final_binary: Option<u64>) {
        #[cfg(test)]
        tests::record_comparison(self.work, final_binary);
        if final_binary.is_some_and(|binary| self.work < binary) {
            plan.multiway = Some(std::sync::Arc::new(self.description));
        }
    }
}

pub(crate) fn price(
    step: &MatchStep,
    original: CompletedJoin<'_>,
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    estimates: &join::estimates::JoinEstimates,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<PricedMultiway>> {
    #[cfg(test)]
    tests::record_before(budget, original.work);
    // analyze_join has proved total predicates, fresh entity variables (no
    // incoming rebinding), and one occurrence of each named Relationship.
    // Preserve that proof rather than admitting a second pattern analyzer.
    if original.plan.factorized.is_some()
        || original.work.is_none()
        || !scope.complete_consumer
        || !estimates.multiway_possible(step.parts.len())
    {
        return Ok(None);
    }
    let Some(description) = topology::describe(step, scope, context, statistics, budget)? else {
        #[cfg(test)]
        tests::record_stage("shape-refused", 0, budget);
        return Ok(None);
    };
    let required = estimates.multiway_output_work();
    #[cfg(test)]
    tests::record_stage("raw-output", required, budget);
    if !charge(context, budget, required, 0)? {
        return Ok(None);
    }
    let Some((description, work, _)) = work::choose(
        step,
        (&original.plan.stages, original.work),
        description,
        statistics,
        estimates,
        context,
        None,
        budget,
    )?
    else {
        return Ok(None);
    };
    Ok(Some(PricedMultiway { description, work }))
}

pub(crate) fn price_component(
    step: &MatchStep,
    description: MultiwayPlan,
    source: (&join::JoinPlan, &[join::JoinStage], u64),
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    estimates: &join::estimates::JoinEstimates,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<(MultiwayPlan, u64, u64)>> {
    if !charge(context, budget, estimates.multiway_output_work(), 0)? {
        return Ok(None);
    }
    let resident = match crate::query::stream::multiway::planning_prefix_memory_bytes(
        step,
        &description,
        source.0,
        context.control,
    ) {
        Ok(bytes) => bytes,
        Err(error) if error.kind == super::super::QueryErrorKind::Resource => return Ok(None),
        Err(error) => return Err(error),
    };
    let result = work::choose(
        step,
        (source.1, Some(source.2)),
        description,
        statistics,
        estimates,
        context,
        Some(resident),
        budget,
    )?;
    #[cfg(test)]
    if let Some((_, work, _)) = &result {
        tests::record_comparison(*work, Some(source.2));
    }
    Ok(result)
}

fn charge(
    context: &join::JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
    units: usize,
    bytes: usize,
) -> QueryResult<bool> {
    context.control.work(units.max(1) as u64)?;
    Ok(budget.work(units, bytes))
}
