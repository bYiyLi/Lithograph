//! A narrow, schema-proved two-arm factorization in the existing MATCH plan.

use super::*;

mod asp;
mod cost;
mod eligibility;
mod path_bindings;
mod topology;

pub(crate) fn install(
    step: &mut MatchStep,
    scope: &join::JoinClauseScope<'_>,
    context: &join::JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    estimates: &join::estimates::JoinEstimates,
    budget: &mut PlanningBudget,
) -> QueryResult<()> {
    let Some(original) = step.join.as_ref() else {
        return Ok(());
    };
    let Some(description) = eligibility::describe(step, scope, context, budget)? else {
        return Ok(());
    };
    let units = estimates.factor_advice_work(original);
    context.control.work(units as u64)?;
    if !budget.work(units, 0) {
        return Ok(());
    }
    let work = estimates.factor_candidate_work(original, &description);
    if !cost::beneficial(step, &description, statistics, work, context.control)? {
        return Ok(());
    }
    if let Some(plan) = step.join.as_mut().and_then(std::sync::Arc::get_mut) {
        if description.asp.is_some() && !asp::install(plan, &description) {
            return Ok(());
        }
        plan.factorized = Some(std::sync::Arc::new(description));
    }
    Ok(())
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
