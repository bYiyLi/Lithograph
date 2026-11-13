//! Price a complete executable cycle-first plan, including its real tail.

use super::*;
use crate::query::plan::multiway as producer;

pub(super) struct Candidate {
    stages: Vec<JoinStage>,
    component: crate::query::plan::MultiwayPlan,
    cost: Cost,
}

impl Candidate {
    pub(super) fn winning_work(&self, binary: Cost) -> Option<u64> {
        self.cost
            .beats(binary)
            .then(|| paid_binary_work(self.cost))
            .flatten()
    }

    pub(super) fn install(self, plan: &mut JoinPlan, binary: Cost) {
        #[cfg(test)]
        eprintln!(
            "S8 hybrid final cost={:?} binary={binary:?} winner={}",
            self.cost,
            self.cost.beats(binary)
        );
        if plan.multiway.is_none() && self.cost.beats(binary) {
            plan.stages = self.stages;
            plan.multiway = Some(Arc::new(self.component));
        }
    }
}

pub(super) fn price(
    pattern: (&MatchStep, &JoinClauseScope<'_>),
    graph: &Graph,
    completed: &JoinPlan,
    estimates: &JoinEstimates,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &crate::query::stats::PlannerStatistics,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Candidate>> {
    let (step, scope) = pattern;
    if !scope.complete_consumer || !estimates.multiway_possible(step.parts.len()) {
        return Ok(None);
    }
    let Some(description) = producer::describe_component(step, scope, context, statistics, budget)?
    else {
        return Ok(None);
    };
    let count = description.edges.len();
    if count == 0 || count >= graph.parts {
        return Ok(None);
    }
    let mask = description
        .edges
        .iter()
        .fold(0u128, |mask, edge| mask | (1_u128 << edge.part));
    let Some(mut stages) = stage_storage(
        graph.parts,
        completed.predicates.len(),
        context.control,
        budget,
    )?
    else {
        return Ok(None);
    };
    let mut search = SearchContext {
        templates: &completed.templates,
        estimates,
        control: context.control,
        prefix_memory: None,
    };
    let Some((prefix, binary)) = greedy::fill(
        graph,
        &search,
        &mut stages[..count],
        mask,
        (0, Cost::root()),
        budget,
    )?
    else {
        return Ok(None);
    };
    let Some(work) = paid_binary_work(binary) else {
        return Ok(None);
    };
    let Some((component, work, resident)) = producer::price_component(
        step,
        description,
        (completed, &stages[..count], work),
        context,
        statistics,
        estimates,
        budget,
    )?
    else {
        return Ok(None);
    };
    let Some(prefix_cost) = component_cost(binary, work, resident) else {
        return Ok(None);
    };
    let Some(memory) = prefix_memory((step, &component), (mask, resident), context, budget)? else {
        return Ok(None);
    };
    search.prefix_memory = Some(memory);
    finish(
        graph,
        &search,
        stages,
        (count, prefix, prefix_cost),
        completed,
        component,
        budget,
    )
}

fn prefix_memory(
    pattern: (&MatchStep, &crate::query::plan::MultiwayPlan),
    state: (u128, u64),
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<HashPrefixMemory>> {
    let (step, component) = pattern;
    let (mask, resident) = state;
    // Plain component Nodes have no Index expressions: the helper walks
    // roots once, then each Node/Relationship output name once.
    let units = component
        .nodes
        .len()
        .saturating_mul(2)
        .saturating_add(component.edges.len());
    if !charge(budget, context.control, units, 0)? {
        return Ok(None);
    }
    let (yielded, drained) = crate::query::stream::multiway::planning_prefix_phase_memory(
        step,
        component,
        context.control,
        resident,
    )?;
    Ok(Some(HashPrefixMemory {
        mask,
        yielded,
        drained,
    }))
}

fn finish(
    graph: &Graph,
    search: &SearchContext<'_, '_>,
    mut stages: Vec<JoinStage>,
    start: (usize, u128, Cost),
    completed: &JoinPlan,
    component: crate::query::plan::MultiwayPlan,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Candidate>> {
    let (count, mask, prefix_cost) = start;
    // The second initial leaf walk and complete predicate schedule are paid
    // before they run. No new ledger or reused binary-tail approximation.
    let units = graph
        .parts
        .saturating_mul(schedule_work(&completed.predicates).saturating_add(1));
    if !charge(budget, search.control, units, 0)? {
        return Ok(None);
    }
    let Some((_, cost)) = greedy::fill(
        graph,
        search,
        &mut stages[count..],
        greedy::all_parts(graph.parts) & !mask,
        (mask, prefix_cost),
        budget,
    )?
    else {
        return Ok(None);
    };
    schedule(&mut stages, &completed.predicates);
    #[cfg(test)]
    eprintln!("S8 hybrid completed mask={mask:#x} multiway_prefix={prefix_cost:?} full={cost:?}");
    Ok(Some(Candidate {
        stages,
        component,
        cost,
    }))
}

fn component_cost(binary: Cost, work: u64, resident: u64) -> Option<Cost> {
    Some(Cost {
        // Existing W work includes its resident-page term. Cost::score adds
        // memory pages itself, so carry only physical work into io/cpu.
        io: Some(work.checked_sub(resident / 4096)?),
        cpu: Some(0),
        rows: Some(binary.rows?),
        memory: resident,
        temp_bytes: Some(0),
    })
}
