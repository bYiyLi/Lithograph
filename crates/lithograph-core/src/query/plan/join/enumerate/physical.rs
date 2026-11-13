//! Physical memo is deliberately left-deep. Two multi-Part topology children
//! never enter it as a fictitious bushy plan which the cursor cannot execute.
use super::topology::ConnectedPair;
use super::*;

#[derive(Clone, Copy)]
struct Entry {
    cost: Cost,
    previous: u128,
    choice: Choice,
}

pub(super) fn search(
    graph: &Graph,
    plan: &JoinPlan,
    estimates: &JoinEstimates,
    pairs: &[ConnectedPair],
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<(Vec<Choice>, Cost)>> {
    let context = SearchContext {
        templates: &plan.templates,
        estimates,
        control,
        prefix_memory: None,
    };
    if !charge(
        budget,
        control,
        graph.parts,
        graph.parts.saturating_mul(std::mem::size_of::<Choice>()),
    )? {
        return Ok(None);
    }
    let mut order = Vec::with_capacity(graph.parts);
    let Some(mut memo) = singletons(graph.parts, &context, budget)? else {
        return Ok(None);
    };
    for pair in pairs {
        if pair.left.is_power_of_two()
            && !extend(pair.right, pair.left, &context, &mut memo, budget)?
        {
            return Ok(None);
        }
        if pair.right.is_power_of_two()
            && !extend(pair.left, pair.right, &context, &mut memo, budget)?
        {
            return Ok(None);
        }
    }
    let all = (1_u128 << graph.parts) - 1;
    let Some(final_entry) = memo.get(&all).copied() else {
        return Ok(Some((
            order,
            Cost {
                io: None,
                cpu: None,
                rows: None,
                memory: 0,
                temp_bytes: None,
            },
        )));
    };
    let mut mask = all;
    while mask != 0 {
        control.work(1)?;
        let entry = memo[&mask];
        order.push(entry.choice);
        mask = entry.previous;
    }
    order.reverse();
    Ok(Some((order, final_entry.cost)))
}

fn singletons(
    parts: usize,
    context: &SearchContext<'_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<BTreeMap<u128, Entry>>> {
    let mut memo = BTreeMap::new();
    for part in 0..parts {
        let Some((choice, mut cost)) = context.best_access(part, 0, false, Cost::root(), budget)?
        else {
            return Ok(None);
        };
        if !charge(
            budget,
            context.control,
            context.estimates.rows_work() + 1,
            256,
        )? {
            return Ok(None);
        }
        cost.rows = context.estimates.rows(1_u128 << part);
        memo.insert(
            1_u128 << part,
            Entry {
                cost,
                previous: 0,
                choice,
            },
        );
    }
    Ok(Some(memo))
}

fn extend(
    outer: u128,
    inner: u128,
    context: &SearchContext<'_, '_>,
    memo: &mut BTreeMap<u128, Entry>,
    budget: &mut PlanningBudget,
) -> QueryResult<bool> {
    let Some(previous) = memo.get(&outer).copied() else {
        return Ok(true);
    };
    let part = inner.trailing_zeros() as usize;
    let Some((choice, mut cost)) =
        context.best_access(part, outer, false, previous.cost, budget)?
    else {
        return Ok(false);
    };
    let union = outer | inner;
    if !charge(
        budget,
        context.control,
        context.estimates.rows_work() + 1,
        if memo.contains_key(&union) { 0 } else { 256 },
    )? {
        return Ok(false);
    }
    cost.rows = context.estimates.rows(union);
    if memo.get(&union).is_none_or(|old| cost.beats(old.cost)) {
        memo.insert(
            union,
            Entry {
                cost,
                previous: outer,
                choice,
            },
        );
    }
    Ok(true)
}
