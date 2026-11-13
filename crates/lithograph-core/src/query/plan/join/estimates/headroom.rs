//! Optional advice must leave room to finish the current clause's real plan.

use super::*;

pub(crate) fn protect_completion_metadata(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    scope: &JoinClauseScope<'_>,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<()> {
    if !budget.clause_scoped() {
        return Ok(());
    }
    let labels = hash::label_slots(step);
    let work = step
        .parts
        .len()
        .saturating_mul(8)
        .saturating_add(analysis.hashes.len().saturating_mul(2))
        .saturating_add(scope.declared_columns.len().saturating_mul(2))
        .saturating_add(labels);
    if work > budget.remaining_states() {
        return Ok(());
    }
    context.control.work(work as u64)?;
    if !budget.work(work, 0) {
        return Ok(());
    }
    let candidates = hash::greedy_candidates(step.parts.len(), analysis);
    let mut bytes = hash::greedy_storage(step.parts.len(), analysis, scope, candidates);
    bytes = bytes.saturating_add(crate::query::plan::factor::headroom_bytes(step, scope));
    if step
        .parts
        .iter()
        .filter(|part| part.relationship.is_some())
        .count()
        >= 3
    {
        bytes = bytes.saturating_add(crate::query::plan::multiway::headroom_bytes(step));
    }
    // This is an admission guard, not a second allocation or a budget debit.
    // Real Greedy and descriptor construction still use the complete ledger.
    budget.reserve_advice_bytes(bytes);
    Ok(())
}
