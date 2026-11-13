//! Optional samples preserve the work of the existing Hash admission and Greedy.

use super::*;

pub(in crate::query::plan::join::estimates) fn advice_headroom_work(
    step: &MatchStep,
    analysis: &JoinAnalysis,
) -> usize {
    if analysis.hashes.is_empty() {
        return 0;
    }
    // Two Node label lengths and three seek slots per Part, plus the borrowed
    // template, Hash provider and local-predicate walks below. No names clone.
    step.parts
        .len()
        .saturating_mul(6)
        .saturating_add(analysis.templates.len())
        .saturating_add(analysis.hashes.len())
        .saturating_add(analysis.predicates.len())
}

pub(in crate::query::plan::join::estimates) fn advice_headroom(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    scope: &JoinClauseScope<'_>,
    base: usize,
) -> usize {
    if analysis.hashes.is_empty() {
        return base;
    }
    // The caller prepays advice_headroom_work before these bounded walks.
    let (indexes, adjacency) =
        analysis
            .templates
            .iter()
            .fold(
                (0usize, 0usize),
                |(indexes, adjacency), template| match template.access {
                    ProbeAccess::IndexEquality { .. } => (indexes.saturating_add(1), adjacency),
                    ProbeAccess::Adjacency { .. } => (indexes, adjacency.saturating_add(1)),
                    _ => (indexes, adjacency),
                },
            );
    let parts = step.parts.len();
    let columns = scope.declared_columns.len();
    let predicates = analysis.predicates.len();
    // Each sampled IndexEquality contributes at most one canonical pair and
    // two property keys; repeated directions cannot exceed predicate count.
    let equalities = indexes.min(predicates);
    let keys = equalities.saturating_mul(2).min(128);
    let filters = analysis
        .predicates
        .iter()
        .filter(|predicate| {
            predicate.guards.len() == 1
                && !matches!(predicate.expression, Expr::Literal(Value::Boolean(true)))
        })
        .count();
    // In a proved MATCH, all named entities occur in declared columns; anonymous
    // None names do not enter the variable map. Extra incoming columns only
    // enlarge this bound. Include filters added after HashInputs::collect.
    let rows = parts
        .saturating_add(columns)
        .saturating_add(keys)
        .saturating_add(equalities.saturating_mul(keys.saturating_mul(2).saturating_add(3)))
        .saturating_add(filters);
    let seeks = step.parts.iter().fold(0usize, |slots, part| {
        slots
            .saturating_add(usize::from(part.start.index_seek.is_some()))
            .saturating_add(usize::from(
                part.end
                    .as_ref()
                    .is_some_and(|node| node.index_seek.is_some()),
            ))
            .saturating_add(usize::from(
                part.relationship
                    .as_ref()
                    .is_some_and(|rel| rel.index_seek.is_some()),
            ))
    });
    base.saturating_add(inspection_work(parts, analysis.hashes.len(), columns))
        .saturating_add(collection_work(
            parts,
            analysis.hashes.len(),
            columns,
            indexes,
            predicates,
            label_slots(step),
        ))
        .saturating_add(greedy_work(
            parts,
            columns,
            hash_alternatives(analysis),
            rows,
            predicates,
        ))
        .saturating_add(post_sample_work(parts, seeks, adjacency, indexes))
}

fn inspection_work(parts: usize, hashes: usize, columns: usize) -> usize {
    parts.saturating_add(hashes).saturating_add(columns)
}

fn collection_work(
    parts: usize,
    hashes: usize,
    variables: usize,
    distributions: usize,
    predicates: usize,
    labels: usize,
) -> usize {
    parts
        .saturating_mul(variables.saturating_add(1))
        // Retain up to two Node facts and one RID fact per Part; also pay
        // for the endpoint-name comparison before skipping a self-loop fact.
        .saturating_add(parts.saturating_mul(4))
        .saturating_add(
            hashes.saturating_mul(parts).saturating_mul(
                distributions
                    .saturating_add(2)
                    .saturating_mul(predicates.saturating_add(4)),
            ),
        )
        .saturating_add(
            hashes
                .saturating_mul(2)
                .saturating_mul(distributions)
                .saturating_mul(labels),
        )
}

fn greedy_work(
    parts: usize,
    variables: usize,
    alternatives: usize,
    rows: usize,
    predicates: usize,
) -> usize {
    parts.saturating_mul(alternatives).saturating_mul(
        parts
            .saturating_add(parts.max(variables))
            .saturating_add(14)
            .saturating_add(rows)
            .saturating_add(predicates),
    )
}

fn post_sample_work(parts: usize, seeks: usize, adjacency: usize, indexes: usize) -> usize {
    // Static collection still compares and retains its remaining seeks; then
    // the access cross-check, bound-source advice and distribution memo each
    // charge the same prepare ledger before the real Hash admission begins.
    let comparisons = seeks
        .checked_add(1)
        .and_then(|next| seeks.checked_mul(next))
        .map_or(usize::MAX, |work| work / 2);
    comparisons
        .saturating_add(parts.saturating_mul(2).saturating_mul(seeks))
        .saturating_add(adjacency)
        .saturating_add(indexes)
}

pub(super) fn admit(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    estimates: &JoinEstimates,
    distributions: usize,
    budget: &mut PlanningBudget,
    headroom: usize,
    scope: &JoinClauseScope<'_>,
) -> bool {
    let parts = step.parts.len();
    let hashes = analysis.hashes.len();
    let inspection = inspection_work(parts, hashes, scope.declared_columns.len());
    if budget.exhausted() || inspection.saturating_add(headroom) > budget.remaining_states() {
        return false;
    }
    // A later byte/candidate refusal still pays for its real borrowed walks.
    if !budget.work(inspection, 0) {
        return false;
    }
    // Size names and labels in one already paid Part walk. No endpoint-name
    // equality is performed until the accepted collection owns these bytes.
    let (bytes, labels) = collection_bytes(step, hashes);
    let work = collection_work(
        parts,
        hashes,
        estimates.variables.len(),
        distributions,
        analysis.predicates.len(),
        labels,
    );
    let greedy_candidates = greedy_candidates(parts, analysis);
    let greedy_work = greedy_work(
        parts,
        estimates.variables.len(),
        hash_alternatives(analysis),
        estimates.rows_work(),
        analysis.predicates.len(),
    );
    let greedy_bytes = greedy_storage(parts, analysis, scope, greedy_candidates);
    #[cfg(test)]
    crate::query::plan::budget::trace::event(format_args!(
        "S8 hash-admit parts={parts} hashes={hashes} states={} bytes={} work={work} headroom={headroom} greedy_work={greedy_work} required_states={} remaining_states={} bytes_needed={} remaining_bytes={} candidates_needed={greedy_candidates} remaining_candidates={}",
        budget.states,
        budget.bytes,
        work.saturating_add(headroom).saturating_add(greedy_work),
        budget.remaining_states(),
        bytes.saturating_add(greedy_bytes),
        budget.remaining_bytes(),
        budget.remaining_candidates()
    ));
    if work.saturating_add(headroom).saturating_add(greedy_work) > budget.remaining_states()
        || bytes.saturating_add(greedy_bytes) > budget.remaining_bytes()
        || greedy_candidates > budget.remaining_candidates()
    {
        return false;
    }
    budget.work(work, bytes)
}

fn collection_bytes(step: &MatchStep, hashes: usize) -> (usize, usize) {
    let parts = step.parts.len();
    let retained = hashes
        .saturating_mul(parts)
        .saturating_mul(
            4 * std::mem::size_of::<HashCandidate>()
                + 2 * std::mem::size_of::<Option<HashKeyAdvice>>()
                + 4 * std::mem::size_of::<usize>(),
        )
        .saturating_add(parts.saturating_mul(96))
        .saturating_add(parts.saturating_mul(std::mem::size_of::<Option<u64>>()))
        .saturating_add(std::mem::size_of::<Vec<u64>>())
        .saturating_add(hashes.saturating_mul(std::mem::size_of::<KeyScopes>()))
        .saturating_add(
            parts
                .saturating_mul(3)
                .saturating_mul(std::mem::size_of::<(u128, u64)>()),
        );
    step.parts
        .iter()
        .fold((retained, 0usize), |(bytes, labels), part| {
            let comparison = part
                .start
                .variable
                .as_ref()
                .zip(part.end.as_ref().and_then(|node| node.variable.as_ref()))
                .map_or(0, |(start, end)| start.len().min(end.len()));
            (
                bytes.saturating_add(comparison),
                labels
                    .saturating_add(part.start.label_names.len())
                    .saturating_add(part.end.as_ref().map_or(0, |node| node.label_names.len())),
            )
        })
}

#[cfg(test)]
mod tests;
