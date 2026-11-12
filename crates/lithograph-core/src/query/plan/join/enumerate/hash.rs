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
        let Some(estimate) = hashes.estimate(id, prefix, cost) else {
            continue;
        };
        let candidate = hashes.candidate(id);
        let next = estimate.append(cost, output.flatten());
        if best.as_ref().is_none_or(|(_, old)| next.beats(*old)) {
            *best = Some((
                Choice {
                    part,
                    orientation: candidate.orientation,
                    access: StageAccess::Hash {
                        equality: candidate.equality,
                        inner_side: candidate.inner_side,
                    },
                    cartesian: false,
                },
                next,
            ));
        }
    }
    Ok(true)
}
