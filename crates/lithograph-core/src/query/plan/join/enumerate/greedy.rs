//! Resume the same bounded Greedy from an actually completed prefix.

use super::*;

pub(super) fn fill(
    graph: &Graph,
    search: &SearchContext<'_, '_>,
    stages: &mut [JoinStage],
    allowed: u128,
    start: (u128, Cost),
    budget: &mut PlanningBudget,
) -> QueryResult<Option<(u128, Cost)>> {
    let (mut prefix, mut cost) = start;
    let mut remaining = (0..graph.parts)
        .filter(|part| allowed & !prefix & (1_u128 << part) != 0)
        .try_fold(0u128, |sum, part| {
            search
                .estimates
                .leaf_score(part)
                .map(|score| sum.saturating_add(score))
        });
    for slot in stages {
        let Some((choice, mut next)) =
            select(graph, search, (allowed, prefix), cost, remaining, budget)?
        else {
            return Ok(None);
        };
        set_choice(slot, choice);
        remaining = remaining
            .zip(search.estimates.leaf_score(choice.part))
            .map(|(total, used)| total.saturating_sub(used));
        prefix |= 1_u128 << choice.part;
        if !charge(budget, search.control, search.estimates.rows_work(), 0)? {
            return Ok(None);
        }
        next.rows = search.estimates.rows(prefix);
        cost = next;
    }
    Ok(Some((prefix, cost)))
}

fn select(
    graph: &Graph,
    search: &SearchContext<'_, '_>,
    masks: (u128, u128),
    cost: Cost,
    remaining: Option<u128>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<(Choice, Cost)>> {
    let (allowed, prefix) = masks;
    let mut best = None;
    for part in 0..graph.parts {
        if allowed & !prefix & (1_u128 << part) == 0 {
            continue;
        }
        if !charge(budget, search.control, graph.singleton_work(), 0)? {
            return Ok(None);
        }
        let connected = prefix == 0 || graph.connects_singleton(prefix, part);
        let Some(candidate) = search.best_access(part, prefix, !connected, cost, budget)? else {
            return Ok(None);
        };
        if best
            .as_ref()
            .is_none_or(|old| greedy_better(candidate, *old, remaining, search.estimates))
        {
            best = Some(candidate);
        }
    }
    Ok(best)
}

pub(super) fn all_parts(parts: usize) -> u128 {
    if parts == 128 {
        u128::MAX
    } else {
        (1_u128 << parts) - 1
    }
}
