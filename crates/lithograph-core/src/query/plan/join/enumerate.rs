//! Complete a runnable Greedy before spending the remainder on DPhyp topology.
use super::cost::{Choice, Cost};
use super::estimates::JoinEstimates;
use super::*;
use crate::query::plan::{MatchStep, budget::PlanningBudget};
use crate::query::{ExecutionControl, QueryResult};
use std::collections::BTreeMap;
use std::sync::Arc;

mod hash;
mod physical;
mod topology;
use topology::{Graph, charge};

pub(crate) fn choose_join_order(
    step: &MatchStep,
    scope: &JoinClauseScope<'_>,
    analysis: JoinAnalysis,
    estimates: &JoinEstimates,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Arc<JoinPlan>>> {
    control.check()?;
    if !estimates.ready(step.parts.len()) {
        return Ok(None);
    }
    let Some(graph) = Graph::new(step.parts.len(), &analysis.edges, budget, control)? else {
        return Ok(None);
    };
    let Some((stages, greedy_cost)) = greedy(&graph, &analysis, estimates, control, budget)? else {
        return Ok(None);
    };
    let Some(mut completed) = complete(scope, analysis, stages, control, budget)? else {
        return Ok(None);
    };
    if let Some(stages) = exact(&graph, &completed, greedy_cost, estimates, control, budget)?
        && let Some(plan) = Arc::get_mut(&mut completed)
    {
        plan.stages = stages;
    }
    control.check()?;
    Ok(Some(completed))
}

fn exact(
    graph: &Graph,
    completed: &JoinPlan,
    greedy_cost: Cost,
    estimates: &JoinEstimates,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Vec<JoinStage>>> {
    // Scratch scheduling/backtrace storage is paid before starting exact search.
    let Some(mut scratch) =
        stage_storage(graph.parts, completed.predicates.len(), control, budget)?
    else {
        record(0, false, true);
        return Ok(None);
    };
    if !charge(
        budget,
        control,
        graph
            .parts
            .saturating_mul(schedule_work(&completed.predicates)),
        0,
    )? {
        record(0, false, true);
        return Ok(None);
    }
    let Some(pairs) = topology::enumerate(graph, control, budget)? else {
        record(0, false, true);
        return Ok(None);
    };
    let result = physical::search(graph, completed, estimates, &pairs, control, budget)?;
    if let Some((choices, cost)) = result {
        if cost.beats(greedy_cost) {
            for (slot, choice) in scratch.iter_mut().zip(choices) {
                set_choice(slot, choice);
            }
            schedule(&mut scratch, &completed.predicates);
            record(0, true, false);
            return Ok(Some(scratch));
        }
        record(0, true, false);
    } else {
        record(0, false, true);
    }
    Ok(None)
}

fn record(pairs: usize, exact: bool, fallback: bool) {
    #[cfg(feature = "test-support")]
    crate::performance::record_join_search(pairs, exact, fallback);
    #[cfg(not(feature = "test-support"))]
    let _ = (pairs, exact, fallback);
}

fn stage_storage(
    parts: usize,
    predicates: usize,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Vec<JoinStage>>> {
    let bytes = parts.saturating_mul(
        std::mem::size_of::<JoinStage>()
            .saturating_add(predicates.saturating_mul(std::mem::size_of::<usize>())),
    );
    if !charge(
        budget,
        control,
        parts.saturating_mul(predicates.saturating_add(1)),
        bytes,
    )? {
        return Ok(None);
    }
    Ok(Some(
        (0..parts)
            .map(|part| JoinStage {
                part,
                orientation: PartOrientation::Written,
                access: StageAccess::Existing,
                cartesian: false,
                predicates_after: Vec::with_capacity(predicates),
            })
            .collect(),
    ))
}

fn complete(
    scope: &JoinClauseScope<'_>,
    analysis: JoinAnalysis,
    mut stages: Vec<JoinStage>,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<Arc<JoinPlan>>> {
    let bytes = scope.declared_columns.iter().fold(
        std::mem::size_of::<JoinPlan>() + 2 * std::mem::size_of::<usize>(),
        |bytes, name| bytes.saturating_add(std::mem::size_of::<String>() + name.len()),
    );
    let work = stages
        .len()
        .saturating_mul(schedule_work(&analysis.predicates).saturating_add(1))
        .saturating_add(scope.declared_columns.len());
    if !charge(budget, control, work, bytes)? {
        return Ok(None);
    }
    schedule(&mut stages, &analysis.predicates);
    Ok(Some(Arc::new(JoinPlan {
        clause_start: scope.clause_start,
        snapshot_identity: scope.snapshot_identity,
        declared_columns: scope.declared_columns.to_vec(),
        predicates: analysis.predicates,
        templates: analysis.templates,
        hashes: analysis.hashes,
        stages,
        factorized: None,
    })))
}

fn schedule_work(predicates: &[JoinPredicate]) -> usize {
    predicates.iter().fold(0usize, |work, p| {
        work.saturating_add(p.guards.len().saturating_add(1))
    })
}

fn schedule(stages: &mut [JoinStage], predicates: &[JoinPredicate]) {
    let mut prefix = 0;
    for stage in stages {
        let before = prefix;
        prefix |= 1_u128 << stage.part;
        stage.predicates_after.clear();
        for (id, predicate) in predicates.iter().enumerate() {
            let ready = |mask| predicate.guards.iter().all(|group| group.0 & mask != 0);
            if ready(prefix) && (!ready(before) || before == 0) {
                stage.predicates_after.push(id);
            }
        }
    }
}

fn set_choice(stage: &mut JoinStage, choice: Choice) {
    stage.part = choice.part;
    stage.orientation = choice.orientation;
    stage.access = choice.access;
    stage.cartesian = choice.cartesian;
}

fn greedy(
    graph: &Graph,
    analysis: &JoinAnalysis,
    estimates: &JoinEstimates,
    control: &ExecutionControl<'_>,
    budget: &mut PlanningBudget,
) -> QueryResult<Option<(Vec<JoinStage>, Cost)>> {
    let Some(mut stages) = stage_storage(graph.parts, analysis.predicates.len(), control, budget)?
    else {
        return Ok(None);
    };
    let search = SearchContext {
        templates: &analysis.templates,
        estimates,
        control,
    };
    let mut remaining_score = (0..graph.parts).try_fold(0u128, |sum, part| {
        estimates
            .leaf_score(part)
            .map(|score| sum.saturating_add(score))
    });
    let mut prefix = 0;
    let mut cost = Cost::root();
    for slot in &mut stages {
        let mut best = None;
        for part in 0..graph.parts {
            if prefix & (1_u128 << part) != 0 {
                continue;
            }
            if !charge(budget, control, graph.singleton_work(), 0)? {
                return Ok(None);
            }
            let connected = prefix == 0 || graph.connects_singleton(prefix, part);
            let Some(candidate) = search.best_access(part, prefix, !connected, cost, budget)?
            else {
                return Ok(None);
            };
            let replace = best
                .as_ref()
                .is_none_or(|old| greedy_better(candidate, *old, remaining_score, estimates));
            if replace {
                best = Some(candidate);
            }
        }
        let Some((choice, mut next)) = best else {
            return Ok(None);
        };
        set_choice(slot, choice);
        remaining_score = remaining_score
            .zip(estimates.leaf_score(choice.part))
            .map(|(total, used)| total.saturating_sub(used));
        prefix |= 1_u128 << choice.part;
        if !charge(budget, control, estimates.rows_work(), 0)? {
            return Ok(None);
        }
        next.rows = estimates.rows(prefix);
        cost = next;
    }
    Ok(Some((stages, cost)))
}

fn greedy_better(
    candidate: (Choice, Cost),
    previous: (Choice, Cost),
    remaining: Option<u128>,
    estimates: &JoinEstimates,
) -> bool {
    let (choice, old) = previous;
    if candidate.1.empty() && !old.empty() {
        return true;
    }
    if candidate.1.score().is_none() || old.score().is_none() {
        return false;
    }
    (choice.cartesian && !candidate.0.cartesian && !old.empty())
        || (choice.cartesian == candidate.0.cartesian
            && greedy_score(candidate.0, candidate.1, remaining, estimates)
                .zip(greedy_score(choice, old, remaining, estimates))
                .is_some_and(|(new, old)| new < old))
}

fn greedy_score(
    choice: Choice,
    cost: Cost,
    remaining: Option<u128>,
    estimates: &JoinEstimates,
) -> Option<u128> {
    let tail = remaining?.saturating_sub(estimates.leaf_score(choice.part)?);
    Some(
        cost.score()?
            .saturating_add(u128::from(cost.rows?.expected).saturating_mul(tail)),
    )
}

pub(super) struct SearchContext<'a, 'ctl> {
    templates: &'a [ParameterTemplate],
    estimates: &'a JoinEstimates,
    control: &'a ExecutionControl<'ctl>,
}

impl SearchContext<'_, '_> {
    fn best_access(
        &self,
        part: usize,
        prefix: u128,
        cartesian: bool,
        cost: Cost,
        budget: &mut PlanningBudget,
    ) -> QueryResult<Option<(Choice, Cost)>> {
        let (templates, estimates, control) = (self.templates, self.estimates, self.control);
        let mut best = None;
        for orientation in [PartOrientation::Written, PartOrientation::Reversed] {
            let choice = Choice {
                part,
                orientation,
                access: StageAccess::Existing,
                cartesian,
            };
            if !consider(
                choice,
                (estimates, prefix),
                cost,
                &mut best,
                budget,
                control,
            )? {
                return Ok(None);
            }
        }
        for &id in estimates.probe_ids(part) {
            let template = &templates[id];
            if !charge(budget, control, 1, 0)? {
                return Ok(None);
            }
            if template.part != part
                || template.source_guard.0 & prefix == 0
                || !estimates.probe_allowed(id, prefix)
            {
                continue;
            }
            let orientation = if template.target == ProbeTarget::EndNode {
                PartOrientation::Reversed
            } else {
                PartOrientation::Written
            };
            let choice = Choice {
                part,
                orientation,
                access: StageAccess::Probe(id),
                cartesian,
            };
            if !consider(
                choice,
                (estimates, prefix),
                cost,
                &mut best,
                budget,
                control,
            )? {
                return Ok(None);
            }
        }
        if !hash::consider(self, part, prefix, cost, &mut best, budget)? {
            return Ok(None);
        }
        Ok(best)
    }
}

fn consider(
    choice: Choice,
    input: (&JoinEstimates, u128),
    cost: Cost,
    best: &mut Option<(Choice, Cost)>,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let (estimates, prefix) = input;
    let Some(access) = estimates.access(choice.part, choice.orientation, choice.access, prefix)
    else {
        return Ok(true);
    };
    control.work(1)?;
    if !budget.candidate() {
        return Ok(false);
    }
    let next = cost.append(access);
    if best.as_ref().is_none_or(|(_, old)| next.beats(*old)) {
        *best = Some((choice, next));
    }
    Ok(true)
}
