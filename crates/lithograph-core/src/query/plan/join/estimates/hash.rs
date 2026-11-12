//! Hash candidates use only the raw independent producer and collected advice.
//! Candidate lookup performs no SQLite read, property sampling, or rebuild.
use super::hash_cohort::full_cohort;
use super::hash_keys::{HashKeyAdvice, key_advice, key_matches};
use super::hash_work::{HashBuild, HashProbe, hash_work};
use super::*;
use crate::query::plan::join::cost::{Cost, HashEstimate};
use crate::query::schema::IndexDistribution;
use crate::query::stream::{hash_layout, hash_resident_target, hash_table_peak_bytes};

#[derive(Debug)]
pub(in crate::query::plan::join) struct HashCandidate {
    pub(super) part: usize,
    pub(in crate::query::plan::join) orientation: PartOrientation,
    pub(in crate::query::plan::join) equality: usize,
    pub(in crate::query::plan::join) inner_side: HashOperandSide,
    probe_guard: u128,
    build: HashBuild,
    target: u64,
    quota: bool,
    probe_property: bool,
}

#[derive(Debug, Default)]
struct KeyScopes {
    left: Vec<Option<HashKeyAdvice>>,
    right: Vec<Option<HashKeyAdvice>>,
}

#[derive(Debug, Default)]
pub(in crate::query::plan::join) struct HashInputs {
    candidates: Vec<HashCandidate>,
    ids: Vec<Vec<usize>>,
    dependencies: Vec<u128>,
    encoded: Vec<u64>,
    keys: Vec<KeyScopes>,
    incoming_unknown: bool,
}

impl HashInputs {
    pub(super) fn collect(
        step: &MatchStep,
        analysis: &JoinAnalysis,
        context: &JoinPlanningContext<'_, '_, '_>,
        estimates: &JoinEstimates,
        distributions: &BTreeMap<&str, Option<IndexDistribution>>,
        budget: &mut PlanningBudget,
        placement: (usize, &JoinClauseScope<'_>),
    ) -> QueryResult<Self> {
        let (headroom, scope) = placement;
        if analysis.hashes.is_empty()
            || !admit(
                step,
                analysis,
                estimates,
                distributions.len(),
                budget,
                headroom,
                scope,
            )
        {
            return Ok(Self::default());
        }
        let mut result = Self {
            candidates: Vec::with_capacity(
                4usize
                    .saturating_mul(step.parts.len())
                    .saturating_mul(analysis.hashes.len()),
            ),
            ids: (0..step.parts.len())
                .map(|_| Vec::with_capacity(4usize.saturating_mul(analysis.hashes.len())))
                .collect(),
            dependencies: Vec::with_capacity(step.parts.len()),
            encoded: Vec::with_capacity(step.parts.len()),
            incoming_unknown: !scope.incoming.is_empty(),
            ..Self::default()
        };
        result.collect_parts(step, analysis, context, estimates)?;
        result.keys = collect_keys(step, analysis, context, distributions);
        for (equality, hash) in analysis.hashes.iter().enumerate() {
            for side in [HashOperandSide::Left, HashOperandSide::Right] {
                let (build, _) = hash.keys(side);
                for part in 0..step.parts.len() {
                    context.control.work(1)?;
                    if build.providers.0 & (1_u128 << part) != 0 {
                        result.collect_candidate(
                            (part, equality, side),
                            step,
                            hash,
                            context,
                            estimates,
                        )?;
                    }
                }
            }
        }
        Ok(result)
    }

    fn collect_parts(
        &mut self,
        step: &MatchStep,
        analysis: &JoinAnalysis,
        context: &JoinPlanningContext<'_, '_, '_>,
        estimates: &JoinEstimates,
    ) -> QueryResult<()> {
        for (id, part) in step.parts.iter().enumerate() {
            context.control.work(1)?;
            let layout = hash_layout(part, &analysis.hashes[0], HashOperandSide::Left)?;
            self.encoded.push(layout.encoded_row_bytes);
            self.dependencies.push(
                estimates
                    .variables
                    .iter()
                    .filter(|variable| variable.providers & (1_u128 << id) != 0)
                    .fold(0, |mask, variable| mask | variable.providers),
            );
        }
        Ok(())
    }

    fn collect_candidate(
        &mut self,
        identity: (usize, usize, HashOperandSide),
        step: &MatchStep,
        equality: &HashEquality,
        context: &JoinPlanningContext<'_, '_, '_>,
        estimates: &JoinEstimates,
    ) -> QueryResult<()> {
        let (part, equality_id, side) = identity;
        let (build, probe) = equality.keys(side);
        let layout = hash_layout(&step.parts[part], equality, side)?;
        let target = hash_resident_target(context.control.resources(), 0, 0, layout.headroom_bytes)
            .min(layout.soft_cap);
        for orientation in [PartOrientation::Written, PartOrientation::Reversed] {
            let access = match orientation {
                PartOrientation::Written => Some(estimates.parts[part].written),
                PartOrientation::Reversed => estimates.parts[part].reversed,
            };
            let Some(access) = access else { continue };
            let peak = access
                .rows
                .and_then(|rows| hash_table_peak_bytes(rows.expected, &layout));
            self.ids[part].push(self.candidates.len());
            self.candidates.push(HashCandidate {
                part,
                orientation,
                equality: equality_id,
                inner_side: side,
                probe_guard: probe.providers.0,
                build: HashBuild {
                    access,
                    layout,
                    peak,
                    key_property: build.property.is_some(),
                },
                target,
                quota: context.control.resources().limit().is_some(),
                probe_property: probe.property.is_some(),
            });
        }
        Ok(())
    }

    pub(in crate::query::plan::join) fn ids(&self, part: usize) -> &[usize] {
        self.ids.get(part).map_or(&[], Vec::as_slice)
    }

    pub(in crate::query::plan::join) fn candidate(&self, id: usize) -> &HashCandidate {
        &self.candidates[id]
    }

    pub(in crate::query::plan::join) fn allowed(&self, id: usize, prefix: u128) -> bool {
        let candidate = &self.candidates[id];
        prefix != 0
            && candidate.probe_guard & prefix != 0
            && self.dependencies[candidate.part] & prefix == 0
    }

    /// The prefix width walk and bounded routing arithmetic are charged once
    /// for each actually considered Hash alternative in the same search ledger.
    pub(in crate::query::plan::join) fn work(&self) -> usize {
        self.encoded.len().saturating_add(12)
    }

    pub(in crate::query::plan::join) fn estimate(
        &self,
        id: usize,
        prefix: u128,
        cost: Cost,
    ) -> Option<HashEstimate> {
        let candidate = &self.candidates[id];
        let keys = &self.keys[candidate.equality];
        let (build_keys, probe_keys) = match candidate.inner_side {
            HashOperandSide::Left => (&keys.left, &keys.right),
            HashOperandSide::Right => (&keys.right, &keys.left),
        };
        let outer = prefix
            .is_power_of_two()
            .then(|| probe_keys[prefix.trailing_zeros() as usize].as_ref())
            .flatten();
        let inner = build_keys[candidate.part].as_ref();
        let matches = key_matches(cost.rows, candidate.build.access.rows, outer, inner)?;
        let encoded_bytes = self.prefix_bytes(prefix);
        let extra_headroom = encoded_bytes
            .saturating_mul(2)
            .saturating_add(4096)
            .saturating_sub(candidate.build.layout.headroom_bytes);
        let mut target = if candidate.quota {
            candidate
                .target
                .saturating_sub(cost.memory)
                .saturating_sub(extra_headroom)
        } else {
            // The executor's default soft target belongs to each Hash owner;
            // it is not a hidden second global query-memory quota.
            candidate.target
        };
        if self.incoming_unknown && candidate.build.peak.is_none_or(|peak| peak > target) {
            return None;
        }
        if candidate.quota && candidate.build.peak.is_none_or(|peak| peak > target) {
            target = target.saturating_sub(crate::query::spill::grace_resident_bound());
        }
        let (partition_passes, hot_passes) = routing(candidate, inner, target);
        hash_work(
            candidate.build,
            HashProbe {
                matches,
                encoded_bytes,
                key_property: candidate.probe_property,
                resident_target: target,
                partition_passes,
                hot_passes,
            },
        )
    }

    fn prefix_bytes(&self, prefix: u128) -> u64 {
        self.encoded
            .iter()
            .enumerate()
            .filter(|(part, _)| prefix & (1_u128 << part) != 0)
            .fold(0_u64, |bytes, (_, width)| bytes.saturating_add(*width))
    }
}

fn admit(
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
    let inspection = parts
        .saturating_add(hashes)
        .saturating_add(scope.declared_columns.len());
    if budget.exhausted()
        || inspection.saturating_add(headroom)
            > crate::query::plan::budget::MAX_STATES.saturating_sub(budget.states)
    {
        return false;
    }
    // Even a later byte/candidate refusal pays for the borrowed masks and
    // column-name walk performed below; no refused request gets free traversal.
    if !budget.work(inspection, 0) {
        return false;
    }
    let work = parts
        .saturating_mul(estimates.variables.len().saturating_add(1))
        .saturating_add(
            hashes.saturating_mul(parts).saturating_mul(
                distributions
                    .saturating_add(2)
                    .saturating_mul(analysis.predicates.len().saturating_add(4)),
            ),
        );
    let labels = step.parts.iter().fold(0usize, |count, part| {
        count
            .saturating_add(part.start.label_names.len())
            .saturating_add(part.end.as_ref().map_or(0, |node| node.label_names.len()))
    });
    // Matching an existing distribution walks real Label names, including
    // unresolved names; those comparisons are not free optional advice.
    let work = work.saturating_add(
        hashes
            .saturating_mul(2)
            .saturating_mul(distributions)
            .saturating_mul(labels),
    );
    let bytes = hashes
        .saturating_mul(parts)
        .saturating_mul(
            4 * std::mem::size_of::<HashCandidate>()
                + 2 * std::mem::size_of::<Option<HashKeyAdvice>>()
                + 4 * std::mem::size_of::<usize>(),
        )
        .saturating_add(parts.saturating_mul(96))
        .saturating_add(hashes.saturating_mul(std::mem::size_of::<KeyScopes>()));
    let alternatives = analysis.hashes.iter().fold(0usize, |count, hash| {
        count.saturating_add(
            (hash.left.providers.0.count_ones() as usize)
                .saturating_add(hash.right.providers.0.count_ones() as usize)
                .saturating_mul(2),
        )
    });
    let greedy_candidates = parts.saturating_mul(
        parts
            .saturating_mul(2)
            .saturating_add(analysis.templates.len())
            .saturating_add(alternatives),
    );
    let greedy_work = parts.saturating_mul(alternatives).saturating_mul(
        parts
            .saturating_add(14)
            .saturating_add(estimates.rows_work())
            .saturating_add(analysis.predicates.len()),
    );
    let greedy_bytes = greedy_storage(parts, analysis, scope, greedy_candidates);
    if work.saturating_add(headroom).saturating_add(greedy_work)
        > crate::query::plan::budget::MAX_STATES.saturating_sub(budget.states)
        || bytes.saturating_add(greedy_bytes)
            > crate::query::plan::budget::MAX_BYTES.saturating_sub(budget.bytes)
        || greedy_candidates
            > crate::query::plan::budget::MAX_CANDIDATES.saturating_sub(budget.candidates)
    {
        return false;
    }
    budget.work(work, bytes)
}

fn greedy_storage(
    parts: usize,
    analysis: &JoinAnalysis,
    scope: &JoinClauseScope<'_>,
    candidates: usize,
) -> usize {
    let columns = scope.declared_columns.iter().fold(0usize, |bytes, name| {
        bytes
            .saturating_add(std::mem::size_of::<String>())
            .saturating_add(name.len())
    });
    parts
        .saturating_mul(16 + std::mem::size_of::<JoinStage>())
        .saturating_add(
            analysis
                .edges
                .len()
                .saturating_mul(std::mem::size_of::<Hyperedge>()),
        )
        .saturating_add(
            parts
                .saturating_mul(analysis.predicates.len())
                .saturating_mul(std::mem::size_of::<usize>()),
        )
        .saturating_add(std::mem::size_of::<JoinPlan>() + 2 * std::mem::size_of::<usize>())
        .saturating_add(columns)
        .saturating_add(candidates.saturating_mul(128))
}

fn collect_keys(
    step: &MatchStep,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    distributions: &BTreeMap<&str, Option<IndexDistribution>>,
) -> Vec<KeyScopes> {
    analysis
        .hashes
        .iter()
        .map(|hash| KeyScopes {
            left: operand_keys(step, &hash.left, analysis, context, distributions),
            right: operand_keys(step, &hash.right, analysis, context, distributions),
        })
        .collect()
}

fn operand_keys(
    step: &MatchStep,
    key: &HashInputKey,
    analysis: &JoinAnalysis,
    context: &JoinPlanningContext<'_, '_, '_>,
    distributions: &BTreeMap<&str, Option<IndexDistribution>>,
) -> Vec<Option<HashKeyAdvice>> {
    let mut keys = vec![None; step.parts.len()];
    for (name, sample) in distributions {
        let Some(sample) = sample else { continue };
        let Some(index) = context.schema.indexes.get(*name) else {
            continue;
        };
        let Some(advice) = key_advice(step, key, index, *sample) else {
            continue;
        };
        for (part, slot) in keys.iter_mut().enumerate() {
            if advice.providers & (1_u128 << part) != 0
                && full_cohort(step, part, key, analysis, context.view)
                && slot.is_none_or(|old: HashKeyAdvice| {
                    advice.ndistinct.expected > old.ndistinct.expected
                })
            {
                *slot = Some(advice);
            }
        }
    }
    keys
}

fn routing(candidate: &HashCandidate, advice: Option<&HashKeyAdvice>, target: u64) -> (u64, u64) {
    let rows = candidate.build.access.rows.map_or(0, |rows| rows.expected);
    let bytes = candidate
        .build
        .peak
        .unwrap_or_else(|| rows.saturating_mul(candidate.build.layout.encoded_row_bytes));
    let mut partitions = 1_u64;
    let mut passes = 1_u64;
    for _ in 0..crate::query::spill::GRACE_MAX_REPARTITION_DEPTH {
        if bytes.div_ceil(partitions) <= target {
            break;
        }
        partitions = partitions.saturating_mul(crate::query::spill::GRACE_PARTITION_FANOUT as u64);
        passes += 1;
    }
    let fixed = candidate
        .build
        .layout
        .entry_inline_bytes
        .saturating_add(candidate.build.layout.bucket_bytes)
        .saturating_add(candidate.build.layout.binding_bytes);
    // With an unknown String width, an observed hot key is conservatively
    // priced as one entry per bounded chunk. It is not given zero heap bytes.
    let row_bytes = candidate
        .build
        .layout
        .key_heap_bytes
        .map_or(target.max(1), |heap| fixed.saturating_add(heap));
    let hot = advice.map_or(0, |advice| {
        advice
            .max_frequency
            .expected
            .max(advice.max_observed_frequency)
            .min(advice.indexed_owners)
            .saturating_mul(row_bytes)
    });
    let chunks = bytes.div_ceil(partitions).max(hot).div_ceil(target.max(1));
    (passes, chunks.max(1))
}

#[cfg(test)]
mod tests;
