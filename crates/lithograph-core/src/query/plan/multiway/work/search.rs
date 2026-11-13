use super::*;

pub(super) struct BestOrder {
    work: u64,
    order: Option<Vec<usize>>,
}

impl BestOrder {
    pub(super) fn new(work: u64) -> Self {
        Self { work, order: None }
    }

    pub(super) fn consider(&mut self, order: Vec<usize>, candidate: Option<u64>) {
        if let Some(work) = candidate
            && work < self.work
        {
            self.work = work;
            self.order = Some(order);
        }
    }

    pub(super) fn finish(self, plan: MultiwayPlan) -> Option<(MultiwayPlan, u64)> {
        self.order.map(|order| (reorder(plan, &order), self.work))
    }
}

pub(super) fn price_order(
    pattern: (&MatchStep, &MultiwayPlan),
    order: &[usize],
    advice: (&PlannerStatistics, &join::estimates::JoinEstimates),
    context: &join::JoinPlanningContext<'_, '_, '_>,
    overhead: Overhead,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<u64>> {
    let (step, plan) = pattern;
    // Root has no adjacency frontier. Each other layer now scans E once for
    // degree, reads, and distinct predecessor identity; no second set scan.
    let units = plan.nodes.len().saturating_add(
        plan.nodes
            .len()
            .saturating_sub(1)
            .saturating_mul(plan.edges.len().saturating_add(4)),
    );
    let extra_labels = plan.nodes.iter().fold(0usize, |count, node| {
        count.saturating_add(node.spec.labels.len().saturating_sub(1))
    });
    let units = units.saturating_add(extra_labels).saturating_add(
        plan.nodes
            .len()
            .saturating_mul(context.view.required_scan_labels().len()),
    );
    #[cfg(test)]
    super::super::tests::record_stage("candidate", units, budget);
    if !charge(context, budget, units, 0)? {
        return Ok(None);
    }
    let candidate = candidate_work(step, plan, order, advice.0, advice.1, context, overhead);
    #[cfg(test)]
    super::super::tests::record_candidate(order[0], candidate, budget);
    Ok(candidate)
}

pub(super) fn alternatives(
    best: &mut BestOrder,
    pattern: (&MatchStep, &MultiwayPlan),
    advice: (&PlannerStatistics, &join::estimates::JoinEstimates),
    context: &join::JoinPlanningContext<'_, '_, '_>,
    overhead: Overhead,
    budget: &mut PlanningBudget,
    first_root: usize,
) -> QueryResult<()> {
    for root in 0..pattern.1.nodes.len() {
        if budget.exhausted() {
            break;
        }
        if root == first_root {
            continue;
        }
        let Some(order) = super::order(pattern.0, pattern.1, root, advice.1, context, budget)?
        else {
            continue;
        };
        let candidate = price_order(pattern, &order, advice, context, overhead, budget)?;
        best.consider(order, candidate);
    }
    Ok(())
}
