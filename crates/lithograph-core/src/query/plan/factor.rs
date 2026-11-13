//! A narrow, schema-proved two-arm factorization in the existing MATCH plan.

use super::*;

mod asp;
mod cost;
mod eligibility;
mod path_bindings;
mod topology;

#[cfg(test)]
mod test_support;

pub(crate) fn headroom_bytes(step: &MatchStep, scope: &join::JoinClauseScope<'_>) -> usize {
    if !scope.complete_consumer
        || step.optional
        || !scope.incoming.is_empty()
        || !eligibility::supported_parts(step)
    {
        return 0;
    }
    step.parts
        .iter()
        .fold(eligibility::metadata_bytes(step, scope), |bytes, part| {
            let nodes = [Some(&part.start), part.end.as_ref()]
                .into_iter()
                .flatten()
                .fold(0usize, |bytes, node| {
                    bytes
                        .saturating_add(node.variable.as_ref().map_or(0, String::len))
                        .saturating_add(
                            node.labels
                                .len()
                                .saturating_mul(std::mem::size_of::<LabelId>()),
                        )
                });
            bytes.saturating_add(nodes).saturating_add(
                part.relationship
                    .as_ref()
                    .and_then(|rel| rel.variable.as_ref())
                    .map_or(0, String::len),
            )
        })
        .saturating_add(
            scope
                .declared_columns
                .iter()
                .map(String::len)
                .sum::<usize>(),
        )
}

pub(crate) fn install(
    step: &mut MatchStep,
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    estimates: &join::estimates::JoinEstimates,
    budget: &mut PlanningBudget,
) -> QueryResult<()> {
    let Some(original) = step.join.as_deref() else {
        return Ok(());
    };
    let Some(priced) = price_inner(
        step,
        (original, None),
        scope,
        context,
        statistics,
        estimates,
        budget,
    )?
    else {
        return Ok(());
    };
    if let Some(plan) = step.join.as_mut().and_then(std::sync::Arc::get_mut) {
        priced.install(plan);
    }
    Ok(())
}

/// A completed Factor candidate for exactly the source stages that priced it.
pub(crate) struct PricedFactor {
    description: FactorizedPlan,
    // Both plans omit the same final output work, as the existing comparison does.
    work: u64,
}

impl PricedFactor {
    pub(crate) fn work(&self) -> u64 {
        self.work
    }

    pub(crate) fn beats(&self, binary: Option<u64>) -> bool {
        binary.is_some_and(|binary| self.work() < binary)
    }

    pub(crate) fn install(self, plan: &mut join::JoinPlan) -> bool {
        if plan.multiway.is_some()
            || (self.description.asp.is_some() && !asp::install(plan, &self.description))
        {
            return false;
        }
        plan.factorized = Some(std::sync::Arc::new(self.description));
        true
    }
}

pub(crate) fn price(
    step: &MatchStep,
    source: (&join::JoinPlan, Option<(u64, u64)>),
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    estimates: &join::estimates::JoinEstimates,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<PricedFactor>> {
    if source.1.is_none() {
        return Ok(None);
    }
    price_inner(step, source, scope, context, statistics, estimates, budget)
}

fn price_inner(
    step: &MatchStep,
    (original, paid): (&join::JoinPlan, Option<(u64, u64)>),
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    estimates: &join::estimates::JoinEstimates,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<PricedFactor>> {
    #[cfg(test)]
    budget.observe(scope.clause_start, "factor-enter");
    if !scope.complete_consumer || original.multiway.is_some() {
        return Ok(None);
    }
    let Some(mut description) = eligibility::describe(step, original, scope, context, budget)?
    else {
        #[cfg(test)]
        budget.observe(scope.clause_start, "factor-description-refused");
        return Ok(None);
    };
    #[cfg(test)]
    budget.observe(scope.clause_start, "factor-described");
    // Flat join order is not an ordering proof for the factor tables: an arm
    // is visited once here instead of once per outer row. Compare both already
    // proved orientations without cloning names, expressions or either table.
    let units = estimates
        .factor_advice_work(original)
        .saturating_mul(2)
        .saturating_add(description.bindings.len().saturating_mul(2))
        .saturating_add(step.parts.len().saturating_mul(2))
        .saturating_add(if paid.is_none() {
            estimates.factor_selected_advice_work(original)
        } else {
            0
        });
    context.control.work(units as u64)?;
    if !budget.work(units, 0) {
        #[cfg(test)]
        budget.observe(scope.clause_start, "factor-cost-refused");
        return Ok(None);
    }
    let Some(flat) = paid.or_else(|| estimates.factor_selected_work(original)) else {
        return Ok(None);
    };
    let work = estimates.factor_candidate_work(original, &description, flat);
    let first = cost::savings(
        step,
        original,
        &description,
        statistics,
        work,
        context.control,
    )?;
    description.swap_arms();
    let work = estimates.factor_candidate_work(original, &description, flat);
    let second = cost::savings(
        step,
        original,
        &description,
        statistics,
        work,
        context.control,
    )?;
    let savings = first.unwrap_or(0).max(second.unwrap_or(0));
    if savings == 0 {
        #[cfg(test)]
        budget.observe(scope.clause_start, "factor-no-savings");
        return Ok(None);
    }
    if second.unwrap_or(0) <= first.unwrap_or(0) {
        description.swap_arms();
    }
    let Some(work) = flat.0.checked_sub(savings) else {
        return Ok(None);
    };
    #[cfg(test)]
    budget.observe(scope.clause_start, "factor-priced");
    Ok(Some(PricedFactor { description, work }))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FactorGroup {
    Shared,
    Accumulate,
    Build,
}

#[derive(Debug, Clone)]
pub(crate) enum FactorBindingKind {
    Node { labels: Vec<LabelId> },
    Relationship { type_id: RelationshipTypeId },
}

#[derive(Debug, Clone)]
pub(crate) struct FactorBinding {
    pub(crate) name: String,
    pub(crate) group: FactorGroup,
    pub(crate) kind: FactorBindingKind,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct FactorHop {
    pub(crate) part: usize,
    pub(crate) orientation: join::PartOrientation,
}

#[derive(Debug, Clone)]
pub(crate) struct FactorArm {
    pub(crate) hops: Vec<FactorHop>,
}

#[derive(Debug, Clone)]
pub(crate) struct FactorizedPlan {
    pub(crate) clause_start: usize,
    pub(crate) shared: String,
    /// A runs from its leaf to the shared endpoint; B starts at that endpoint.
    pub(crate) accumulate: FactorArm,
    pub(crate) build: FactorArm,
    pub(crate) bindings: Vec<FactorBinding>,
    /// Proven by the complete, total shared-key predicate and versioned UNIQUE
    /// definition, never inferred from a statistics count or observed first row.
    pub(crate) single_shared: bool,
    pub(crate) shared_predicate: Expr,
    pub(crate) asp: Option<FactorAsp>,
}

#[derive(Debug, Clone)]
pub(crate) struct FactorAsp {
    pub(crate) part: usize,
    pub(crate) probe_variable: String,
    pub(crate) build_variable: String,
    pub(crate) probe_group: FactorGroup,
}

impl FactorizedPlan {
    fn swap_arms(&mut self) {
        std::mem::swap(&mut self.accumulate, &mut self.build);
        topology::reverse(&mut self.accumulate);
        topology::reverse(&mut self.build);
        for binding in &mut self.bindings {
            binding.group = opposite(binding.group);
        }
        if let Some(asp) = &mut self.asp {
            asp.probe_group = opposite(asp.probe_group);
        }
    }
}

fn opposite(group: FactorGroup) -> FactorGroup {
    match group {
        FactorGroup::Shared => FactorGroup::Shared,
        FactorGroup::Accumulate => FactorGroup::Build,
        FactorGroup::Build => FactorGroup::Accumulate,
    }
}

/// Expected work of the actual old order and the normalized independent arms.
/// All values come from this query's already collected access advice.
#[derive(Debug, Clone, Copy)]
pub(crate) struct FactorWork {
    pub(crate) old: u64,
    pub(crate) output: u64,
    /// Each entry is [rows, SQLite/graph I/O work, CPU work].
    pub(crate) arms: [[u64; 3]; 2],
    pub(crate) point_work: Option<u64>,
}

#[cfg(test)]
mod tests;
