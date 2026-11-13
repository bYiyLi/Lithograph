//! Hash alternatives follow the existing NL/INL baseline and must strictly win.
use super::*;

pub(super) fn consider(
    search: &SearchContext<'_, '_>,
    part: usize,
    prefix: u128,
    cost: Cost,
    best: &mut Option<(Choice, Cost)>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    let hashes = search.estimates.hashes();
    #[cfg(test)]
    crate::query::plan::budget::trace::event(format_args!(
        "S8 hash-select part={part} prefix={prefix:#x} ids={:?} prefix_cost={cost:?} baseline={best:?}",
        hashes.ids(part)
    ));
    let mut output = None;
    for &id in hashes.ids(part) {
        if !charge(budget, search.control, 1, 0)? {
            return Ok(false);
        }
        if !hashes.allowed(id, prefix) {
            continue;
        }
        if output.is_none() {
            if !charge(budget, search.control, search.estimates.rows_work(), 0)? {
                return Ok(false);
            }
            output = Some(search.estimates.rows(prefix | (1_u128 << part)));
            // Both alternatives produce this same subset. Hash is not granted
            // an artificially smaller output-risk interval than the real NL.
            if let Some((_, previous)) = best {
                previous.rows = output.flatten();
            }
        }
        if !charge(budget, search.control, hashes.work(), 0)? || !budget.candidate() {
            return Ok(false);
        }
        let Some(estimate) = hashes.estimate_prefix(id, prefix, cost, search.prefix_memory) else {
            #[cfg(test)]
            crate::query::plan::budget::trace::event(format_args!(
                "S8 hash-estimate id={id} prefix={prefix:#x} unknown"
            ));
            continue;
        };
        let candidate = hashes.candidate(id);
        let next = estimate.append(cost, output.flatten());
        #[cfg(test)]
        crate::query::plan::budget::trace::event(format_args!(
            "S8 hash-estimate id={id} prefix={prefix:#x} estimate={estimate:?} next={next:?} score={:?} baseline_score={:?}",
            next.score(),
            best.as_ref().and_then(|(_, cost)| cost.score())
        ));
        if best.as_ref().is_none_or(|(_, old)| next.beats(*old)) {
            *best = Some((
                Choice {
                    part,
                    orientation: candidate.orientation,
                    access: StageAccess::Hash {
                        equality: candidate.equality,
                        inner_side: candidate.inner_side,
                        dynamic_filter: None,
                    },
                    cartesian: false,
                },
                next,
            ));
        }
        if !consider_dynamic(search, id, prefix, cost, output.flatten(), best, budget)? {
            return Ok(false);
        }
    }
    Ok(true)
}

fn consider_dynamic(
    search: &SearchContext<'_, '_>,
    id: usize,
    prefix: u128,
    cost: Cost,
    output: Option<crate::query::stats::CardinalityEstimate>,
    best: &mut Option<(Choice, Cost)>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    let hashes = search.estimates.hashes();
    if !hashes.has_dynamic(id, prefix) {
        return Ok(true);
    }
    if !charge(budget, search.control, 24, 0)? || !budget.candidate() {
        return Ok(false);
    }
    if let Some((template, next)) = hashes.dynamic_estimate(id, prefix, cost, output) {
        #[cfg(test)]
        eprintln!(
            "S6 DF aggregate candidate={id} df_score={:?} df_cost={next:?} best_access={:?} best_score={:?} best_cost={:?}",
            next.score(),
            best.as_ref().map(|(choice, _)| choice.access),
            best.as_ref().and_then(|(_, old)| old.score()),
            best.as_ref().map(|(_, old)| old),
        );
        if best.as_ref().is_none_or(|(_, old)| next.beats(*old)) {
            let candidate = hashes.candidate(id);
            *best = Some((
                Choice {
                    part: candidate.part,
                    orientation: candidate.orientation,
                    access: StageAccess::Hash {
                        equality: candidate.equality,
                        inner_side: candidate.inner_side,
                        dynamic_filter: Some(template),
                    },
                    cartesian: false,
                },
                next,
            ));
        }
    }
    Ok(true)
}
