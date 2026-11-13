//! One snapshot-scoped collection before enumeration. No cost candidate runs SQL.
use super::cost::{AccessEstimate, expectation, multiply};
use super::*;
use crate::query::plan::budget::PlanningBudget;
use crate::query::plan::{Direction, MatchStep, NodeSpec, PatternPart};
use crate::query::stats::{CardinalityEstimate, PlannerStatistics};
use crate::query::{QueryErrorKind, QueryResult};

mod collect;
mod factor;
use collect::{distribution, existing_index};
mod degree;
use degree::{DegreeSamples, collect_degree_samples};
mod dynamic_filter;
mod hash;
mod hash_cohort;
mod hash_keys;
mod hash_work;
mod headroom;
pub(crate) use headroom::protect_completion_metadata;
mod key_facts;
mod multiway;
use hash::HashInputs;

#[derive(Debug)]
struct PartEstimate {
    written: AccessEstimate,
    reversed: Option<AccessEstimate>,
    output: Option<CardinalityEstimate>,
}

#[derive(Debug)]
struct VariableDomain {
    providers: u128,
    count: Option<u64>,
}
#[derive(Debug)]
struct LocalFilter {
    providers: u128,
    divisor: u64,
    empty: bool,
}

#[derive(Debug)]
struct EqualityDomain {
    left: u128,
    right: u128,
    distinct: u64,
    left_key: usize,
    right_key: usize,
}

/// A generated shared-node template has one source group per oriented endpoint.
/// Keep its cost in a fixed slot so native bound access never scans templates.
#[derive(Debug, Clone, Copy)]
struct BoundSource {
    guard: u128,
    access: AccessEstimate,
}

#[derive(Debug, Clone, Copy, Default)]
struct BoundSources {
    written: Option<BoundSource>,
    reversed: Option<BoundSource>,
}

/// Lifetimes proved for the first Hash tail of a fresh cyclic component.
#[derive(Clone, Copy)]
pub(crate) struct HashPrefixMemory {
    pub(crate) mask: u128,
    pub(crate) yielded: u64,
    pub(crate) drained: u64,
}

/// Opaque to the execution layer: only B interprets cardinalities and costs.
#[derive(Debug, Default)]
pub(crate) struct JoinEstimates {
    parts: Vec<PartEstimate>,
    probes: Vec<Option<AccessEstimate>>,
    probe_bound_targets: Vec<u128>,
    bindings: Vec<(u128, u128)>,
    bound: Vec<(Option<AccessEstimate>, Option<AccessEstimate>)>,
    bound_sources: Vec<BoundSources>,
    probe_ids: Vec<Vec<usize>>,
    variables: Vec<VariableDomain>,
    node_variables: usize,
    relationship_parts: usize,
    equalities: Vec<EqualityDomain>,
    equality_keys: usize,
    filters: Vec<LocalFilter>,
    hashes: HashInputs,
}

impl JoinEstimates {
    pub(super) fn ready(&self, parts: usize) -> bool {
        self.parts.len() == parts
    }

    pub(super) fn access(
        &self,
        part: usize,
        orientation: PartOrientation,
        access: StageAccess,
        prefix: u128,
    ) -> Option<AccessEstimate> {
        match access {
            StageAccess::Existing => match orientation {
                PartOrientation::Written => {
                    if self.bindings[part].0 & prefix != 0 {
                        self.bound_access(part, orientation, prefix)
                            .or(Some(self.parts[part].written))
                    } else {
                        Some(self.parts[part].written)
                    }
                }
                PartOrientation::Reversed => {
                    if self.bindings[part].1 & prefix != 0 {
                        self.bound_access(part, orientation, prefix)
                            .or(self.parts[part].reversed)
                    } else {
                        self.parts[part].reversed
                    }
                }
            },
            StageAccess::Probe(template) => self.probes[template],
            StageAccess::Hash { .. } => None,
        }
    }

    fn bound_access(
        &self,
        part: usize,
        orientation: PartOrientation,
        prefix: u128,
    ) -> Option<AccessEstimate> {
        let (native, source) = match orientation {
            PartOrientation::Written => (self.bound[part].0, self.bound_sources[part].written),
            PartOrientation::Reversed => (self.bound[part].1, self.bound_sources[part].reversed),
        };
        // None denotes the earlier Relationship ID/Index producer, not a node
        // access which an Adjacency template is allowed to redefine.
        let native = native?;
        Some(
            source
                .filter(|source| source.guard & prefix != 0)
                .map_or(native, |source| source.access),
        )
    }

    pub(super) fn probe_allowed(&self, template: usize, prefix: u128) -> bool {
        self.probe_bound_targets[template] & prefix == 0
    }

    pub(super) fn probe_ids(&self, part: usize) -> &[usize] {
        &self.probe_ids[part]
    }

    pub(super) fn hashes(&self) -> &HashInputs {
        &self.hashes
    }

    pub(super) fn leaf_score(&self, part: usize) -> Option<u128> {
        let score = |a: AccessEstimate| Some(u128::from(a.io?) + u128::from(a.cpu?));
        let written = score(self.parts[part].written);
        match (written, self.parts[part].reversed.and_then(score)) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    pub(super) fn rows_work(&self) -> usize {
        self.parts.len()
            + self.filters.len()
            + self.variables.len()
            + self.equality_keys
            + self
                .equalities
                .len()
                .saturating_mul(self.equality_keys.saturating_mul(2).saturating_add(3))
    }

    /// A subset has one estimate regardless of the memo path which reached it.
    pub(super) fn rows(&self, mask: u128) -> Option<CardinalityEstimate> {
        let mut rows = self.join_selectivity(mask, self.raw_part_rows(mask)?)?;
        if let Some(upper) = self.hashes.output_upper(mask) {
            rows.upper = rows.upper.min(upper);
            rows.expected = rows.expected.min(rows.upper);
            rows.lower = rows.lower.min(rows.upper);
        }
        for filter in &self.filters {
            if mask & filter.providers != 0 {
                if filter.empty {
                    return Some(CardinalityEstimate::exact(0));
                }
                rows.expected = rows.expected.div_ceil(filter.divisor);
                rows.lower = 0;
            }
        }
        rows.lower = if mask.is_power_of_two() {
            rows.lower
        } else {
            0
        };
        Some(rows)
    }

    fn raw_part_rows(&self, mask: u128) -> Option<CardinalityEstimate> {
        let mut rows = Some(CardinalityEstimate::exact(1));
        for (part, estimate) in self.parts.iter().enumerate() {
            if mask & (1_u128 << part) != 0 {
                rows = multiply(rows, estimate.output);
            }
        }
        rows
    }
    fn join_selectivity(
        &self,
        mask: u128,
        rows: CardinalityEstimate,
    ) -> Option<CardinalityEstimate> {
        let mut rows = self.identity_selectivity(mask, rows)?;
        let mut parents = [0u8; 128];
        for (key, parent) in parents.iter_mut().enumerate().take(self.equality_keys) {
            *parent = key as u8;
        }
        for equality in &self.equalities {
            if mask & equality.left == 0 || mask & equality.right == 0 {
                continue;
            }
            let left = root(&parents, equality.left_key);
            let right = root(&parents, equality.right_key);
            if left != right {
                parents[left] = right as u8;
                rows.expected = rows.expected.div_ceil(equality.distinct.max(1));
            }
        }
        Some(rows)
    }

    fn identity_selectivity(
        &self,
        mask: u128,
        mut rows: CardinalityEstimate,
    ) -> Option<CardinalityEstimate> {
        for domain in &self.variables {
            let copies = (mask & domain.providers).count_ones().saturating_sub(1);
            if copies == 0 {
                continue;
            }
            let count = domain.count?.max(1);
            rows.expected = rows.expected.div_ceil(count.saturating_pow(copies));
        }
        Some(rows)
    }
}

pub(crate) fn collect_join_estimates(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    scope: &JoinClauseScope<'_>,
) -> QueryResult<JoinEstimates> {
    context.control.check()?;
    if !admit_collection(step, analysis, context, budget) {
        return Ok(JoinEstimates::default());
    }
    let headroom = step
        .parts
        .len()
        .saturating_mul(
            step.parts
                .len()
                .saturating_mul(7)
                .saturating_add(analysis.templates.len())
                .saturating_add(
                    analysis
                        .predicates
                        .iter()
                        .map(|p| p.guards.len() + 1)
                        .sum::<usize>(),
                ),
        )
        .saturating_add(128);
    let advice_headroom = sampling_headroom(step, analysis, scope, headroom, context, budget)?;
    let static_indexes = collect_static(step, context, budget, advice_headroom)?;
    let degree_samples =
        collect_degree_samples(step, analysis, context, statistics, budget, advice_headroom)?;
    if !budget.work(
        step.parts
            .len()
            .saturating_mul(2)
            .saturating_mul(static_indexes.len()),
        0,
    ) {
        return Ok(JoinEstimates::default());
    }
    let mut estimates = JoinEstimates {
        probe_ids: vec![Vec::new(); step.parts.len()],
        bound_sources: vec![BoundSources::default(); step.parts.len()],
        ..JoinEstimates::default()
    };
    for part in &step.parts {
        context.control.work(1)?;
        let written = existing(
            part,
            false,
            context,
            statistics,
            &static_indexes,
            &degree_samples,
        );
        let reversed = if part.relationship.is_some() && part.end.is_some() {
            Some(existing(
                part,
                true,
                context,
                statistics,
                &static_indexes,
                &degree_samples,
            ))
        } else {
            None
        };
        let output = match (written.rows, reversed.and_then(|e| e.rows)) {
            (Some(a), Some(b)) => Some(if a.expected <= b.expected { a } else { b }),
            (a, b) => a.or(b),
        };
        estimates.bound.push((
            bound_existing(part, false, context, statistics, &degree_samples),
            if part.end.is_some() {
                bound_existing(part, true, context, statistics, &degree_samples)
            } else {
                None
            },
        ));
        estimates.parts.push(PartEstimate {
            written,
            reversed,
            output,
        });
    }
    collect_variables(step, context, statistics, &mut estimates);
    collect_probes(
        step,
        analysis,
        context,
        statistics,
        budget,
        (headroom, advice_headroom, scope),
        (&degree_samples, &mut estimates),
    )?;
    collect_filters(analysis, &mut estimates);
    Ok(estimates)
}

fn sampling_headroom(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    scope: &JoinClauseScope<'_>,
    base: usize,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> QueryResult<usize> {
    let work = hash::advice_headroom_work(step, analysis);
    if work == 0 {
        return Ok(base);
    }
    if work > budget.remaining_states() {
        return Ok(usize::MAX);
    }
    context.control.work(work as u64)?;
    if !budget.work(work, 0) {
        return Ok(usize::MAX);
    }
    Ok(hash::advice_headroom(step, analysis, scope, base))
}

fn admit_collection(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
) -> bool {
    let bytes = step
        .parts
        .len()
        .saturating_mul(768usize.saturating_add(std::mem::size_of::<BoundSources>()))
        .saturating_add(analysis.templates.len().saturating_mul(256))
        .saturating_add(
            analysis
                .predicates
                .len()
                .saturating_mul(std::mem::size_of::<LocalFilter>()),
        );
    if !budget.work(step.parts.len(), 0) {
        return false;
    }
    let labels = step.parts.iter().fold(0usize, |slots, part| {
        slots
            .saturating_add(part.start.labels.len())
            .saturating_add(part.end.as_ref().map_or(0, |end| end.labels.len()))
    });
    let labels = labels.saturating_add(
        step.parts
            .len()
            .saturating_mul(2)
            .saturating_mul(context.view.required_scan_labels().len()),
    );
    if labels > 128 {
        return false;
    }
    let work = step
        .parts
        .len()
        .saturating_mul(8)
        .saturating_add(labels.saturating_mul(4))
        .saturating_add(analysis.templates.len().saturating_mul(2))
        .saturating_add(step.parts.len().saturating_mul(analysis.predicates.len()));
    budget.work(work, bytes)
}

fn node_domain(
    node: &NodeSpec,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
) -> Option<u64> {
    if node.impossible {
        return Some(0);
    }
    if node.id_seek.is_some() {
        return Some(1);
    }
    node.labels
        .iter()
        .chain(context.view.required_scan_labels())
        .filter_map(|id| statistics.label_count(*id))
        .min()
        .or_else(|| (statistics.node_count > 0).then_some(statistics.node_count))
}

fn node_scan_domain(
    node: &NodeSpec,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
) -> Option<u64> {
    if node.impossible {
        return Some(0);
    }
    if node.id_seek.is_some() {
        return Some(1);
    }
    match node.scan_label.or_else(|| context.view.scan_label()) {
        Some(label) => statistics.label_count(label),
        None => (statistics.node_count > 0).then_some(statistics.node_count),
    }
}

fn relationship_domain(part: &PatternPart, statistics: &PlannerStatistics) -> Option<u64> {
    let rel = part.relationship.as_ref()?;
    if rel.impossible {
        return Some(0);
    }
    if rel.id_seek.is_some() {
        return Some(1);
    }
    rel.type_id
        .and_then(|id| statistics.type_counts.get(&id).copied())
        .or_else(|| (statistics.relationship_count > 0).then_some(statistics.relationship_count))
}

fn degree(
    part: &PatternPart,
    domain: Option<u64>,
    statistics: &PlannerStatistics,
) -> Option<CardinalityEstimate> {
    let total = relationship_domain(part, statistics)?;
    let multiplier = if part.relationship.as_ref()?.direction == Direction::Undirected {
        2
    } else {
        1
    };
    Some(expectation(
        total.saturating_mul(multiplier).div_ceil(domain?.max(1)),
        total.saturating_mul(multiplier),
    ))
}

fn existing(
    part: &PatternPart,
    reversed: bool,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    static_indexes: &[(
        &crate::query::schema::StandardIndexSeek,
        Option<CardinalityEstimate>,
    )],
    degree_samples: &DegreeSamples,
) -> AccessEstimate {
    // Relationship identity/index wins before next_start in the actual cursor.
    if let Some(rel) = &part.relationship {
        if rel.id_seek.is_some() {
            return AccessEstimate::stream(Some(expectation(1, 1)), 1);
        }
        if let Some(seek) = &rel.index_seek {
            let rows = static_indexes
                .iter()
                .find(|(request, _)| *request == seek)
                .and_then(|(_, rows)| *rows)
                .or_else(|| relationship_domain(part, statistics).map(|n| expectation(n, n)));
            return AccessEstimate::stream(rows, 2);
        }
    }
    let Some(node) = (if reversed {
        part.end.as_ref()
    } else {
        Some(&part.start)
    }) else {
        return AccessEstimate::stream(None, 0);
    };
    let domain = node_domain(node, context, statistics);
    let scan = node_scan_domain(node, context, statistics);
    let indexed = node
        .index_seek
        .as_ref()
        .is_some_and(|seek| seek.kind != crate::storage::StandardIndexKind::Lookup);
    let observed = node.index_seek.as_ref().and_then(|seek| {
        static_indexes
            .iter()
            .find(|(request, _)| *request == seek)
            .and_then(|(_, rows)| *rows)
    });
    let candidates = if indexed {
        observed.or_else(|| {
            (statistics.node_count > 0)
                .then_some(expectation(statistics.node_count, statistics.node_count))
        })
    } else {
        scan.map(CardinalityEstimate::exact)
    };
    let rows = candidates
        .zip(domain)
        .map(|(candidate, domain)| CardinalityEstimate {
            expected: candidate.expected.min(domain),
            lower: if node
                .labels
                .len()
                .saturating_add(context.view.required_scan_labels().len())
                > 1
                || node.id_seek.is_some()
                || indexed
            {
                0
            } else {
                candidate.lower.min(domain)
            },
            upper: candidate.upper.min(domain),
        });
    let mut access = AccessEstimate::stream(rows, u64::from(indexed));
    access.io = candidates.map(|rows| rows.expected.saturating_add(u64::from(indexed)));
    access.cpu = access.io.map(|io| io.saturating_mul(2));
    if part.relationship.is_some() {
        let fanout = degree_samples
            .existing(part, reversed, context, statistics)
            .or_else(|| degree(part, domain, statistics));
        access.rows = multiply(rows, fanout);
        access.io = scan
            .zip(access.rows)
            .map(|(starts, edges)| starts.saturating_add(edges.expected.saturating_mul(2)));
        access.cpu = access.io.map(|io| io.saturating_mul(2));
    }
    access
}

/// A typed relationship occurrence cannot bind more distinct values at one
/// endpoint than its relationship count. This is an upper bound on this
/// occurrence's key domain, not an observed NDV or a new cardinality sample.
fn endpoint_domain(
    domain: Option<u64>,
    relationship: Option<&crate::query::plan::RelationshipSpec>,
    statistics: &PlannerStatistics,
) -> Option<u64> {
    let domain = domain?;
    let Some(relationship) = relationship else {
        return Some(domain);
    };
    let Some(total) = relationship
        .type_id
        .and_then(|id| statistics.type_counts.get(&id))
        .copied()
    else {
        return Some(domain);
    };
    let bound = if relationship.direction == Direction::Undirected {
        let Some(union) = total.checked_mul(2) else {
            return Some(domain);
        };
        union
    } else {
        total
    };
    Some(domain.min(bound))
}

fn collect_variables(
    step: &MatchStep,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    estimates: &mut JoinEstimates,
) {
    // Borrowed names are used only during this bounded collection, never interned.
    let mut variables: BTreeMap<&str, VariableDomain> = BTreeMap::new();
    let mut node_variables = 0;
    let mut relationship_parts = 0;
    for (id, part) in step.parts.iter().enumerate() {
        relationship_parts += usize::from(part.relationship.is_some());
        for node in std::iter::once(&part.start).chain(part.end.iter()) {
            if let Some(name) = &node.variable {
                let count = endpoint_domain(
                    node_domain(node, context, statistics),
                    part.relationship.as_ref(),
                    statistics,
                );
                let entry = variables.entry(name).or_insert_with(|| {
                    node_variables += 1;
                    VariableDomain {
                        providers: 0,
                        count,
                    }
                });
                entry.providers |= 1_u128 << id;
                entry.count = match (entry.count, count) {
                    (Some(a), Some(b)) => Some(a.max(b)),
                    (a, b) => a.or(b),
                };
            }
        }
        if let Some(rel) = &part.relationship
            && let Some(name) = &rel.variable
        {
            let entry = variables.entry(name).or_insert(VariableDomain {
                providers: 0,
                count: relationship_domain(part, statistics),
            });
            entry.providers |= 1_u128 << id;
        }
    }
    estimates.node_variables = node_variables;
    estimates.relationship_parts = relationship_parts;
    estimates.bindings = step
        .parts
        .iter()
        .map(|part| {
            let providers = |node: &NodeSpec| {
                node.variable
                    .as_deref()
                    .and_then(|name| variables.get(name))
                    .map_or(0, |v| v.providers)
            };
            (
                providers(&part.start),
                part.end.as_ref().map_or(0, providers),
            )
        })
        .collect();
    estimates.variables = variables.into_values().collect();
}

fn collect_probes(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    budget: &mut PlanningBudget,
    placement: (usize, usize, &JoinClauseScope<'_>),
    output: (&DegreeSamples, &mut JoinEstimates),
) -> QueryResult<()> {
    let (headroom, advice_headroom, scope) = placement;
    let (degree_samples, estimates) = output;
    let mut distributions = BTreeMap::new();
    for (id, template) in analysis.templates.iter().enumerate() {
        estimates.probe_ids[template.part].push(id);
        let bound_target = bound_target_providers(step, template, estimates);
        estimates.probe_bound_targets.push(bound_target);
        context.control.work(1)?;
        let part = &step.parts[template.part];
        let node = match template.target {
            ProbeTarget::EndNode => part.end.as_ref(),
            ProbeTarget::StartNode => Some(&part.start),
            ProbeTarget::Relationship => None,
        };
        let mut rows = match &template.access {
            ProbeAccess::Identity { .. } => Some(expectation(1, 1)),
            ProbeAccess::Adjacency { .. } if part.relationship.is_none() => Some(expectation(1, 1)),
            ProbeAccess::Adjacency { .. } => {
                let domain = degree_samples.source_cohort(id);
                degree_samples
                    .source_probe(step, template)
                    .or_else(|| degree_samples.probe(part, template.target, domain))
                    .or_else(|| degree(part, domain.map(|(_, count)| count), statistics))
            }
            ProbeAccess::IndexEquality {
                index_name,
                definition_hash,
                ..
            } => index_probe_rows(
                index_name,
                *definition_hash,
                context,
                budget,
                advice_headroom,
                &mut distributions,
            )?,
        };
        if !matches!(template.access, ProbeAccess::Adjacency { .. })
            && node.is_some()
            && part.relationship.is_some()
        {
            rows = multiply(
                rows,
                degree(
                    part,
                    node.and_then(|node| node_domain(node, context, statistics)),
                    statistics,
                ),
            );
        }
        if !remember_bound_source(estimates, part, template, rows, budget) {
            return Ok(());
        }
        estimates.probes.push(rows.map(|rows| {
            let mut access = AccessEstimate::stream(Some(rows), 2);
            if matches!(template.access, ProbeAccess::IndexEquality { .. }) {
                // Every real outer binding evaluates and copies its property
                // before the parameterized Index cursor is opened.
                access.io = access.io.map(|io| io.saturating_add(1));
                access.cpu = access.cpu.map(|cpu| cpu.saturating_add(3));
            }
            access
        }));
    }
    collect_equalities(step, analysis, &distributions, context.schema, estimates);
    estimates.hashes = HashInputs::collect(
        step,
        analysis,
        context,
        estimates,
        &distributions,
        budget,
        (headroom, scope),
    )?;
    Ok(())
}

fn index_probe_rows<'a>(
    index_name: &'a str,
    definition_hash: HashId,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
    headroom: usize,
    distributions: &mut BTreeMap<&'a str, Option<crate::query::schema::IndexDistribution>>,
) -> QueryResult<Option<CardinalityEstimate>> {
    if !distributions.contains_key(index_name) {
        let sample = distribution(index_name, definition_hash, context, budget, headroom)?;
        distributions.insert(index_name, sample);
    }
    if context
        .schema
        .indexes
        .get(index_name)
        .is_some_and(|index| key_facts::unique(index, context.schema))
    {
        return Ok(Some(expectation(1, 1)));
    }
    Ok(distributions[index_name].map(|sample| {
        let average = sample
            .indexed_owners
            .div_ceil(sample.ndistinct.expected.max(1));
        // A sampled heavy key contributes to risk, not a false uniformity promise.
        expectation(
            average.max(sample.max_frequency.expected / 8),
            sample.indexed_owners,
        )
    }))
}

fn bound_target_providers(
    step: &MatchStep,
    template: &ParameterTemplate,
    estimates: &JoinEstimates,
) -> u128 {
    if matches!(template.access, ProbeAccess::Adjacency { .. })
        && step.parts[template.part].relationship.is_some()
    {
        return 0;
    }
    match template.target {
        ProbeTarget::StartNode => estimates.bindings[template.part].0,
        ProbeTarget::EndNode => estimates.bindings[template.part].1,
        ProbeTarget::Relationship => 0,
    }
}

fn remember_bound_source(
    estimates: &mut JoinEstimates,
    part: &PatternPart,
    template: &ParameterTemplate,
    rows: Option<CardinalityEstimate>,
    budget: &mut PlanningBudget,
) -> bool {
    if part.relationship.is_none() || !matches!(template.access, ProbeAccess::Adjacency { .. }) {
        return true;
    }
    let Some(rows) = rows else {
        return true;
    };
    let (native, slot) = match template.target {
        ProbeTarget::StartNode => (
            estimates.bound[template.part].0,
            &mut estimates.bound_sources[template.part].written,
        ),
        ProbeTarget::EndNode => (
            estimates.bound[template.part].1,
            &mut estimates.bound_sources[template.part].reversed,
        ),
        ProbeTarget::Relationship => return true,
    };
    if native.is_none() {
        return true;
    }
    if !budget.work(1, 0) {
        return false;
    }
    *slot = Some(BoundSource {
        guard: template.source_guard.0,
        access: AccessEstimate::stream(Some(rows), 1),
    });
    true
}

fn collect_equalities(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    distributions: &BTreeMap<&str, Option<crate::query::schema::IndexDistribution>>,
    schema: &crate::storage::SchemaState,
    estimates: &mut JoinEstimates,
) {
    let mut keys = BTreeMap::new();
    let mut seen = BTreeMap::<_, usize>::new();
    for template in &analysis.templates {
        let ProbeAccess::IndexEquality {
            source_variable,
            source_property,
            index_name,
            ..
        } = &template.access
        else {
            continue;
        };
        let Some(sample) = distributions.get(index_name.as_str()).copied().flatten() else {
            continue;
        };
        let target = match template.target {
            ProbeTarget::StartNode => step.parts[template.part].start.variable.as_deref(),
            ProbeTarget::EndNode => step.parts[template.part]
                .end
                .as_ref()
                .and_then(|n| n.variable.as_deref()),
            ProbeTarget::Relationship => step.parts[template.part]
                .relationship
                .as_ref()
                .and_then(|r| r.variable.as_deref()),
        };
        let Some(target) = target else { continue };
        let Some(property) = schema
            .indexes
            .get(index_name)
            .and_then(|index| match &index.target {
                crate::storage::IndexTarget::NodeProperties { properties, .. }
                | crate::storage::IndexTarget::RelationshipProperties { properties, .. } => {
                    properties.first().map(String::as_str)
                }
                _ => None,
            })
        else {
            continue;
        };
        let source = (source_variable.as_str(), source_property.as_str());
        let target_key = (target, property);
        let pair = if source < target_key {
            (source, target_key)
        } else {
            (target_key, source)
        };
        let target_provider = 1_u128 << template.part;
        let (left, right) = if source == pair.0 {
            (template.source_guard.0, target_provider)
        } else {
            (target_provider, template.source_guard.0)
        };
        if let Some(&id) = seen.get(&pair) {
            estimates.equalities[id].left |= left;
            estimates.equalities[id].right |= right;
            estimates.equalities[id].distinct = estimates.equalities[id]
                .distinct
                .max(sample.ndistinct.expected);
            continue;
        }
        if keys.len() > 126 {
            continue;
        }
        let next = keys.len();
        let left_key = *keys.entry(pair.0).or_insert(next);
        let next = keys.len();
        let right_key = *keys.entry(pair.1).or_insert(next);
        seen.insert(pair, estimates.equalities.len());
        estimates.equalities.push(EqualityDomain {
            left,
            right,
            distinct: sample.ndistinct.expected,
            left_key,
            right_key,
        });
        estimates.equality_keys = keys.len();
    }
}

fn collect_static<'a>(
    step: &'a MatchStep,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<
    Vec<(
        &'a crate::query::schema::StandardIndexSeek,
        Option<CardinalityEstimate>,
    )>,
> {
    let mut samples = Vec::new();
    for part in &step.parts {
        for seek in std::iter::once(&part.start)
            .chain(part.end.iter())
            .filter_map(|node| node.index_seek.as_ref())
            .chain(
                part.relationship
                    .as_ref()
                    .and_then(|rel| rel.index_seek.as_ref()),
            )
        {
            context.control.work(1)?;
            if !budget.work(samples.len().saturating_add(1), 64) {
                return Ok(samples);
            }
            if samples.iter().any(|(old, _)| *old == seek) {
                continue;
            }
            let sample = existing_index(seek, context, budget, headroom)?;
            samples.push((seek, sample));
        }
    }
    Ok(samples)
}

fn root(parents: &[u8; 128], mut key: usize) -> usize {
    while parents[key] as usize != key {
        key = parents[key] as usize;
    }
    key
}

#[cfg(test)]
mod tests;

fn collect_filters(analysis: &JoinAnalysis, estimates: &mut JoinEstimates) {
    for predicate in &analysis.predicates {
        if predicate.guards.len() != 1 {
            continue;
        }
        let (divisor, empty) = match &predicate.expression {
            Expr::Literal(Value::Boolean(false) | Value::Null) => (1, true),
            Expr::Literal(Value::Boolean(true)) => continue,
            Expr::Binary(crate::query::expression::BinaryOp::Equal, _, _) => (16, false),
            _ => (4, false),
        };
        estimates.filters.push(LocalFilter {
            providers: predicate.guards[0].0,
            divisor,
            empty,
        });
    }
    for (id, part) in estimates.parts.iter_mut().enumerate() {
        for filter in &estimates.filters {
            if filter.providers & (1_u128 << id) == 0 {
                continue;
            }
            let apply = |access: &mut AccessEstimate| {
                if let Some(mut rows) = access.rows {
                    if filter.empty {
                        rows = CardinalityEstimate::exact(0);
                    } else {
                        rows.expected = rows.expected.div_ceil(filter.divisor);
                        rows.lower = 0;
                    }
                    access.rows = Some(rows);
                }
            };
            apply(&mut part.written);
            if let Some(reversed) = &mut part.reversed {
                apply(reversed);
            }
            if let Some(bound) = &mut estimates.bound[id].0 {
                apply(bound);
            }
            if let Some(bound) = &mut estimates.bound[id].1 {
                apply(bound);
            }
            if let Some(source) = &mut estimates.bound_sources[id].written {
                apply(&mut source.access);
            }
            if let Some(source) = &mut estimates.bound_sources[id].reversed {
                apply(&mut source.access);
            }
        }
    }
}

fn bound_existing(
    part: &PatternPart,
    reversed: bool,
    context: &JoinPlanningContext<'_, '_, '_>,
    statistics: &PlannerStatistics,
    degree_samples: &DegreeSamples,
) -> Option<AccessEstimate> {
    if part
        .relationship
        .as_ref()
        .is_some_and(|rel| rel.id_seek.is_some() || rel.index_seek.is_some())
    {
        return None;
    }
    let node = if reversed {
        part.end.as_ref()?
    } else {
        &part.start
    };
    let rows = if part.relationship.is_some() {
        degree_samples
            .existing(part, reversed, context, statistics)
            .or_else(|| degree(part, node_domain(node, context, statistics), statistics))
    } else {
        Some(expectation(1, 1))
    };
    Some(AccessEstimate::stream(rows, 1))
}

#[cfg(test)]
mod diagnostics;
