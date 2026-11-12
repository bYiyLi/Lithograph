//! A singleton Probe frontier followed by a complete typed-key seek union.
//! All input advice is collected before enumeration; no candidate runs SQL.
use super::hash_cohort::full_cohort;
use super::hash_keys::key_advice;
use super::*;
use crate::query::plan::join::cost::Cost;
use crate::query::schema::IndexDistribution;
use crate::storage::{IndexDefinition, IndexTarget, StandardIndexKind};

#[derive(Debug, Clone, Copy)]
pub(super) struct DynamicInput {
    pub(super) build: usize,
    pub(super) probe: usize,
    pub(super) equality: usize,
    pub(super) inner_side: HashOperandSide,
    pub(super) template: TemplateId,
    build_rows: CardinalityEstimate,
    probe_rows: CardinalityEstimate,
    distinct: CardinalityEstimate,
}

pub(super) fn collect(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    estimates: &JoinEstimates,
    distributions: &BTreeMap<&str, Option<IndexDistribution>>,
    budget: &mut PlanningBudget,
    placement: (usize, &JoinClauseScope<'_>),
) -> QueryResult<Vec<DynamicInput>> {
    let (headroom, scope) = placement;
    if !shape(step, analysis, context.view, scope) {
        return Ok(Vec::new());
    }
    let work = analysis
        .templates
        .len()
        .saturating_mul(distributions.len().saturating_add(4));
    let (headroom, candidates, storage) = search_reserve(analysis, estimates, scope, headroom);
    if work.saturating_add(headroom)
        > crate::query::plan::budget::MAX_STATES.saturating_sub(budget.states)
        || candidates > crate::query::plan::budget::MAX_CANDIDATES.saturating_sub(budget.candidates)
        || storage.saturating_add(2 * std::mem::size_of::<DynamicInput>())
            > crate::query::plan::budget::MAX_BYTES.saturating_sub(budget.bytes)
        || !budget.work(work, 2 * std::mem::size_of::<DynamicInput>())
    {
        return Ok(Vec::new());
    }
    let mut output = Vec::with_capacity(2);
    for (template, target) in analysis.templates.iter().enumerate() {
        context.control.work(1)?;
        if let Some(input) = candidate(
            (template, target),
            step,
            analysis,
            context,
            estimates,
            distributions,
            (budget, headroom),
        )? {
            if !output
                .iter()
                .any(|old: &DynamicInput| old.build == input.build && old.probe == input.probe)
            {
                output.push(input);
            }
            if output.len() == 2 {
                break;
            }
        }
    }
    Ok(output)
}

fn shape(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    view: &ResolvedGraphView,
    scope: &JoinClauseScope<'_>,
) -> bool {
    step.parts.len() == 2
        && analysis.hashes.len() == 1
        && analysis.predicates.len() == 1
        && scope.incoming.is_empty()
        && view == &ResolvedGraphView::default()
}

fn search_reserve(
    analysis: &JoinAnalysis,
    estimates: &JoinEstimates,
    scope: &JoinClauseScope<'_>,
    headroom: usize,
) -> (usize, usize, usize) {
    // Exactly two singleton providers give at most four written/reversed Hash
    // choices. Preserve the original Greedy reservation plus both optional DF
    // directions; refusing this advice never invalidates the completed search.
    let hashes = 4usize;
    let candidates = 2usize.saturating_mul(4 + analysis.templates.len() + hashes);
    let original = 2usize.saturating_mul(hashes).saturating_mul(
        18usize
            .saturating_add(estimates.rows_work())
            .saturating_add(analysis.predicates.len()),
    );
    let dynamic = 2 * hashes * 2 + 2 * 2 * 24;
    let storage = super::hash::greedy_storage(2, analysis, scope, candidates.saturating_add(4));
    (
        headroom.saturating_add(original).saturating_add(dynamic),
        candidates.saturating_add(4),
        storage,
    )
}

fn candidate(
    template: (usize, &ParameterTemplate),
    step: &MatchStep,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    estimates: &JoinEstimates,
    distributions: &BTreeMap<&str, Option<IndexDistribution>>,
    budget: (&mut PlanningBudget, usize),
) -> QueryResult<Option<DynamicInput>> {
    let (id, template) = template;
    let ProbeAccess::IndexEquality {
        index_name,
        source_variable,
        source_property,
        ..
    } = &template.access
    else {
        return Ok(None);
    };
    if analysis.hashes[0].family != HashFamily::Number || template.target != ProbeTarget::StartNode
    {
        return Ok(None);
    }
    let Some(probe_sample) = probe_index(index_name, context, distributions, budget.0, budget.1)?
    else {
        return Ok(None);
    };
    for inner_side in [HashOperandSide::Left, HashOperandSide::Right] {
        let Some(build) = matching_side(
            step,
            analysis,
            context.view,
            template,
            inner_side,
            source_variable,
            source_property,
        ) else {
            continue;
        };
        let (build_key, _) = analysis.hashes[0].keys(inner_side);
        if let Some(distinct) =
            build_distinct(step, build_key, context, distributions, budget.0, budget.1)?
        {
            let Some(build_rows) = estimates.parts[build].output else {
                continue;
            };
            let Some(probe_rows) = estimates.parts[template.part].output else {
                continue;
            };
            if probe_rows.lower == probe_rows.upper
                && probe_rows.upper == probe_sample.indexed_owners
                && distinct.upper <= build_rows.upper
            {
                return Ok(Some(DynamicInput {
                    build,
                    probe: template.part,
                    equality: 0,
                    inner_side,
                    template: id,
                    build_rows,
                    probe_rows,
                    distinct,
                }));
            }
        }
    }
    Ok(None)
}

fn probe_index(
    name: &str,
    context: &JoinPlanningContext<'_, '_, '_>,
    distributions: &BTreeMap<&str, Option<IndexDistribution>>,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<Option<IndexDistribution>> {
    let Some(index) = context.schema.indexes.get(name) else {
        return Ok(None);
    };
    let Some(sample) = distributions.get(name).copied().flatten() else {
        return Ok(None);
    };
    if !super::key_facts::unique(index, context.schema)
        || !node_index(index)
        || sample.generation_anchor != context.snapshot.commit()
    {
        return Ok(None);
    }
    let Some(facts) = super::key_facts::collect(index, context, budget, headroom)? else {
        return Ok(None);
    };
    Ok((facts.unique && facts.integer && facts.exists).then_some(sample))
}

fn matching_side(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    view: &ResolvedGraphView,
    template: &ParameterTemplate,
    inner_side: HashOperandSide,
    source_variable: &str,
    source_property: &str,
) -> Option<usize> {
    let (build, probe) = analysis.hashes[0].keys(inner_side);
    if build.variable != source_variable
        || build.property.as_deref() != Some(source_property)
        || build.providers.0.count_ones() != 1
        || probe.providers.0 != 1_u128 << template.part
    {
        return None;
    }
    let ordinal = build.providers.0.trailing_zeros() as usize;
    (ordinal != template.part
        && ordinal < 2
        && full_cohort(step, ordinal, build, analysis, view)
        && full_cohort(step, template.part, probe, analysis, view))
    .then_some(ordinal)
}

fn build_distinct(
    step: &MatchStep,
    key: &HashInputKey,
    context: &JoinPlanningContext<'_, '_, '_>,
    distributions: &BTreeMap<&str, Option<IndexDistribution>>,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<Option<CardinalityEstimate>> {
    for (name, sample) in distributions {
        context.control.work(1)?;
        let Some(sample) = sample else { continue };
        let Some(index) = context.schema.indexes.get(*name) else {
            continue;
        };
        if !node_index(index) || sample.generation_anchor != context.snapshot.commit() {
            continue;
        }
        let Some(advice) = key_advice(step, key, index, *sample) else {
            continue;
        };
        let Some(facts) = super::key_facts::collect(index, context, budget, headroom)? else {
            return Ok(None);
        };
        if facts.integer && advice.ndistinct.upper <= DYNAMIC_FILTER_MAX_KEYS as u64 {
            return Ok(Some(advice.ndistinct));
        }
    }
    Ok(None)
}

fn node_index(index: &IndexDefinition) -> bool {
    index.kind == StandardIndexKind::Range
        && matches!(&index.target, IndexTarget::NodeProperties { properties, .. } if properties.len() == 1)
}

impl DynamicInput {
    pub(super) fn workspace(self) -> Option<u64> {
        // Both old/new key capacities during growth, retained owner union and
        // one encoded seek/page workspace remain live with the Hash owner.
        self.distinct
            .upper
            .checked_mul(96)?
            .checked_add(512 + 64 * 1024)
    }

    pub(super) fn prefix(self, original: Cost, workspace: u64) -> Option<Cost> {
        let n = self.probe_rows.expected;
        if original.rows != Some(self.probe_rows)
            || original.io != Some(n)
            || original.cpu != Some(n.saturating_mul(2))
        {
            return None;
        }
        let first = n.min(DYNAMIC_FILTER_INITIAL_PAGE as u64);
        let tail = n.saturating_sub(first).min(self.distinct.expected);
        let upper_tail = self
            .probe_rows
            .upper
            .saturating_sub(first)
            .min(self.distinct.upper);
        let probes = CardinalityEstimate {
            expected: first.saturating_add(tail),
            lower: first,
            upper: first.saturating_add(upper_tail),
        };
        let d = self.distinct.expected;
        let comparisons = u64::from(64 - d.max(1).leading_zeros());
        // Reuse the Hash key's existing digest. With load at most one half,
        // uniform linear probing costs 1.5 successful / 2.5 absent positions.
        // Include each new key's two absent walks, all geometric-growth replay
        // (<2D positions), and entry/bucket initialization/copy work. The real
        // collector also caps every walk at 16; long collisions abandon the
        // complete advisory set rather than turning this into unbounded work.
        let collector = self
            .build_rows
            .expected
            .saturating_mul(3)
            .saturating_add(d.saturating_mul(19))
            .div_ceil(2);
        let union_sort = d.saturating_mul(comparisons);
        Some(Cost {
            io: Some(
                first
                    .saturating_add(d.saturating_mul(2))
                    .saturating_add(tail),
            ),
            cpu: Some(
                first
                    .saturating_mul(2)
                    .saturating_add(d.saturating_mul(2))
                    .saturating_add(tail.saturating_mul(2))
                    .saturating_add(collector)
                    .saturating_add(union_sort),
            ),
            rows: Some(probes),
            memory: original.memory.saturating_add(workspace),
            temp_bytes: original.temp_bytes,
        })
    }
}

#[cfg(test)]
mod tests;
