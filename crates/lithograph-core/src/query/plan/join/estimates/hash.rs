//! Hash candidates use only the raw independent producer and collected advice.
//! Candidate lookup performs no SQLite read, property sampling, or rebuild.
use super::hash_cohort::full_cohort;
use super::hash_keys::{HashKeyAdvice, key_advice, key_matches};
use super::hash_work::{HashBuild, HashProbe, hash_work};
use super::*;
use crate::query::plan::join::cost::{Cost, HashEstimate};
use crate::query::schema::IndexDistribution;
use crate::query::stream::{hash_layout, hash_table_peak_bytes};

mod admission;
use admission::admit;
pub(super) use admission::{advice_headroom, advice_headroom_work};

#[derive(Debug)]
pub(in crate::query::plan::join) struct HashCandidate {
    pub(in crate::query::plan::join) part: usize,
    pub(in crate::query::plan::join) orientation: PartOrientation,
    pub(in crate::query::plan::join) equality: usize,
    pub(in crate::query::plan::join) inner_side: HashOperandSide,
    probe_guard: u128,
    build: HashBuild,
    /// Unclamped hard-grant remainder after the already-live query claims and
    /// this layout's headroom. None keeps the per-owner default soft target.
    available: Option<u64>,
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
    encoded: Vec<Option<u64>>,
    encoding_scratch: Vec<u64>,
    prefix_bindings: Option<Vec<(u128, u64)>>,
    relationship_parts: u128,
    keys: Vec<KeyScopes>,
    incoming_unknown: bool,
    output_upper: Option<u64>,
    dynamic: Vec<super::dynamic_filter::DynamicInput>,
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
        #[cfg(test)]
        crate::query::plan::budget::trace::event(format_args!(
            "S8 hash-collect parts={} hashes={} headroom={headroom} remaining_states={} remaining_bytes={}",
            step.parts.len(),
            analysis.hashes.len(),
            budget.remaining_states(),
            budget.remaining_bytes()
        ));
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
            #[cfg(test)]
            crate::query::plan::budget::trace::event(format_args!("S8 hash-collect declined"));
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
            encoding_scratch: Vec::with_capacity(step.parts.len()),
            incoming_unknown: !scope.incoming.is_empty(),
            ..Self::default()
        };
        result.collect_parts(step, analysis, context, estimates)?;
        result.keys = collect_keys(step, analysis, context, distributions);
        result.output_upper = pair_upper(step, estimates, &result.keys);
        if let [left, right] = estimates.parts.as_slice() {
            result.dynamic = super::dynamic_filter::collect(
                step,
                analysis,
                context,
                &[left.output, right.output],
                distributions,
                budget,
                (headroom, estimates.rows_work(), scope),
            )?;
        }
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
        #[cfg(test)]
        crate::query::plan::budget::trace::event(format_args!(
            "S8 hash-collected ids={:?}",
            result.ids
        ));
        Ok(result)
    }

    fn collect_parts(
        &mut self,
        step: &MatchStep,
        analysis: &JoinAnalysis,
        context: &JoinPlanningContext<'_, '_, '_>,
        estimates: &JoinEstimates,
    ) -> QueryResult<()> {
        self.prefix_bindings = prefix_bindings(step, estimates);
        for (id, part) in step.parts.iter().enumerate() {
            context.control.work(1)?;
            let layout = hash_layout(part, &analysis.hashes[0], HashOperandSide::Left)?;
            self.encoded.push(layout.encoded_row_bytes);
            self.encoding_scratch.push(layout.encoding_scratch_bytes);
            if part.relationship.is_some() {
                self.relationship_parts |= 1_u128 << id;
            }
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
        let resources = context.control.resources();
        let available = resources.limit().map(|limit| {
            limit
                .saturating_sub(resources.used())
                .saturating_sub(layout.headroom_bytes)
        });
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
                available,
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
        let parts = self.encoded.len();
        parts
            .saturating_add(
                self.prefix_bindings
                    .as_ref()
                    .map_or(parts, |vars| parts.max(vars.len())),
            )
            .saturating_add(12)
            .saturating_add(self.dynamic.len())
    }

    pub(super) fn output_upper(&self, mask: u128) -> Option<u64> {
        (mask == 3).then_some(self.output_upper).flatten()
    }

    pub(in crate::query::plan::join) fn estimate(
        &self,
        id: usize,
        prefix: u128,
        cost: Cost,
    ) -> Option<HashEstimate> {
        self.estimate_with_memory(id, prefix, cost, 0, None)
    }

    pub(in crate::query::plan::join) fn estimate_prefix(
        &self,
        id: usize,
        prefix: u128,
        cost: Cost,
        memory: Option<HashPrefixMemory>,
    ) -> Option<HashEstimate> {
        self.estimate_with_memory(
            id,
            prefix,
            cost,
            0,
            memory.filter(|memory| memory.mask == prefix),
        )
    }

    fn estimate_with_memory(
        &self,
        id: usize,
        prefix: u128,
        cost: Cost,
        extra_memory: u64,
        memory: Option<HashPrefixMemory>,
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
        let encoded_bytes = self.prefix_bytes(prefix)?;
        let prefix_scratch = self.prefix_scratch(prefix)?;
        let extra_headroom = prefix_scratch
            .saturating_mul(2)
            .saturating_add(4096)
            .saturating_sub(candidate.build.layout.headroom_bytes);
        let outside = cost
            .memory
            .saturating_add(extra_memory)
            .saturating_add(extra_headroom);
        let base_target = candidate.resident_target(outside, false);
        let (target, partition) = spill_target(
            candidate,
            (base_target, outside),
            memory,
            prefix_scratch,
            extra_memory,
            self.incoming_unknown,
        )?;
        #[cfg(test)]
        crate::query::plan::budget::trace::event(format_args!(
            "S8 hash-target id={id} prefix={prefix:#x} available={:?} peak={:?} layout_headroom={} soft_cap={} prefix_memory={} prefix_scratch={prefix_scratch} extra_headroom={extra_headroom} outside={outside} base_target={base_target} final_target={target} encoded_bytes={encoded_bytes} incoming_unknown={}",
            candidate.available,
            candidate.build.peak,
            candidate.build.layout.headroom_bytes,
            candidate.build.layout.soft_cap,
            cost.memory,
            self.incoming_unknown
        ));
        let (partition_passes, hot_passes) = routing(candidate, inner, target);
        let mut estimate = hash_work(
            candidate.build,
            HashProbe {
                matches,
                encoded_bytes,
                key_property: candidate.probe_property,
                resident_target: target,
                partition_passes,
                hot_passes,
            },
        )?;
        if let Some(partition) = partition {
            estimate.resident = estimate.resident.max(partition.saturating_sub(cost.memory));
        }
        Some(estimate)
    }

    pub(in crate::query::plan::join) fn dynamic_estimate(
        &self,
        id: usize,
        prefix: u128,
        cost: Cost,
        output: Option<CardinalityEstimate>,
    ) -> Option<(TemplateId, Cost)> {
        let candidate = &self.candidates[id];
        let input = self.dynamic.iter().find(|input| {
            input.build == candidate.part
                && input.equality == candidate.equality
                && input.inner_side == candidate.inner_side
                && prefix == 1_u128 << input.probe
        })?;
        let workspace = input.workspace()?;
        #[cfg(test)]
        self.log_dynamic_target(id, prefix, cost, workspace, input);
        let estimate = self.estimate_with_memory(id, prefix, cost, workspace, None)?;
        // Retain the unfiltered full-domain K estimate. KeySet overlap is not
        // an independent selectivity proof and cannot discount match merging.
        let filtered = input.prefix(cost, workspace)?;
        #[cfg(test)]
        eprintln!("S6 DF aggregate estimate={estimate:?} filtered_prefix={filtered:?}");
        Some((input.template, estimate.append(filtered, output)))
    }

    #[cfg(test)]
    fn log_dynamic_target(
        &self,
        id: usize,
        prefix: u128,
        cost: Cost,
        workspace: u64,
        input: &super::dynamic_filter::DynamicInput,
    ) {
        let candidate = &self.candidates[id];
        let extra_headroom = self
            .prefix_scratch(prefix)
            .unwrap_or(u64::MAX)
            .saturating_mul(2)
            .saturating_add(4096)
            .saturating_sub(candidate.build.layout.headroom_bytes);
        let outside = cost
            .memory
            .saturating_add(workspace)
            .saturating_add(extra_headroom);
        let base_target = candidate.resident_target(outside, false);
        let spilling = candidate.build.peak.is_none_or(|peak| peak > base_target);
        let final_target = candidate.resident_target(outside, spilling);
        eprintln!(
            "S6 DF aggregate candidate={id} input={input:?} build_peak={:?} soft_cap={} hard_available={:?} prefix_memory={} df_workspace={workspace} extra_headroom={extra_headroom} outside={outside} base_target={base_target} spilling={spilling} final_target={final_target}",
            candidate.build.peak, candidate.build.layout.soft_cap, candidate.available, cost.memory,
        );
    }

    pub(in crate::query::plan::join) fn has_dynamic(&self, id: usize, prefix: u128) -> bool {
        let candidate = &self.candidates[id];
        self.dynamic.iter().any(|input| {
            input.build == candidate.part
                && input.equality == candidate.equality
                && input.inner_side == candidate.inner_side
                && prefix == 1_u128 << input.probe
        })
    }

    fn prefix_bytes(&self, prefix: u128) -> Option<u64> {
        self.encoded
            .iter()
            .enumerate()
            .filter(|(part, _)| prefix & (1_u128 << part) != 0)
            .try_fold(0_u64, |bytes, (_, width)| {
                Some(bytes.saturating_add((*width)?))
            })
    }

    fn prefix_scratch(&self, prefix: u128) -> Option<u64> {
        if !self.incoming_unknown
            && let Some(bindings) = &self.prefix_bindings
        {
            let (mut count, mut names) = (0usize, 0u64);
            for &(providers, bytes) in bindings {
                if providers & prefix != 0 {
                    count = count.checked_add(1)?;
                    names = names.checked_add(bytes)?;
                }
            }
            let capacity = if count == 0 {
                0
            } else {
                count.checked_mul(2)?.max(4)
            };
            let bytes = crate::query::stream::identity_binding_row_bytes(
                count,
                names,
                (self.relationship_parts & prefix).count_ones() as usize,
                capacity,
            )
            .ok()?;
            return crate::query::spill::identity_binding_encoding_scratch(bytes, count, names)
                .ok();
        }
        Some(
            self.encoding_scratch
                .iter()
                .enumerate()
                .filter(|(part, _)| prefix & (1_u128 << part) != 0)
                .fold(0_u64, |bytes, (_, width)| bytes.saturating_add(*width)),
        )
    }
}

fn prefix_bindings(step: &MatchStep, estimates: &JoinEstimates) -> Option<Vec<(u128, u64)>> {
    if estimates.bindings.len() != step.parts.len() {
        return None;
    }
    let mut bindings = Vec::with_capacity(step.parts.len().saturating_mul(3));
    for (part, pattern) in step.parts.iter().enumerate() {
        let (start, end) = estimates.bindings[part];
        append_binding(
            &mut bindings,
            part,
            pattern.start.variable.as_deref(),
            start,
        )?;
        let end_name = pattern
            .end
            .as_ref()
            .and_then(|node| node.variable.as_deref());
        if end_name != pattern.start.variable.as_deref() {
            append_binding(&mut bindings, part, end_name, end)?;
        }
        if let Some(name) = pattern
            .relationship
            .as_ref()
            .and_then(|rel| rel.variable.as_deref())
        {
            bindings.push((1_u128 << part, name.len() as u64));
        }
    }
    Some(bindings)
}

fn append_binding(
    bindings: &mut Vec<(u128, u64)>,
    part: usize,
    name: Option<&str>,
    providers: u128,
) -> Option<()> {
    if let Some(name) = name {
        if providers == 0 {
            return None;
        }
        if providers.trailing_zeros() as usize == part {
            bindings.push((providers, name.len() as u64));
        }
    }
    Some(())
}

impl HashCandidate {
    fn resident_target(&self, outside: u64, spilling: bool) -> u64 {
        self.available
            .map_or(self.build.layout.soft_cap, |available| {
                let grace = if spilling {
                    crate::query::spill::grace_resident_bound()
                } else {
                    0
                };
                available
                    .saturating_sub(outside)
                    .saturating_sub(grace)
                    .min(self.build.layout.soft_cap)
            })
    }
}

fn spill_target(
    candidate: &HashCandidate,
    state: (u64, u64),
    memory: Option<HashPrefixMemory>,
    scratch: u64,
    extra: u64,
    incoming_unknown: bool,
) -> Option<(u64, Option<u64>)> {
    let (base, outside) = state;
    let spilling = candidate.build.peak.is_none_or(|peak| peak > base);
    if incoming_unknown && spilling {
        return None;
    }
    if candidate.available.is_none() || !spilling {
        return Some((base, None));
    }
    if let Some(memory) = memory {
        let (target, peak) = grace_prefix_target(candidate, memory, scratch, extra)?;
        Some((target, Some(peak)))
    } else {
        Some((candidate.resident_target(outside, true), None))
    }
}

fn grace_prefix_target(
    candidate: &HashCandidate,
    memory: HashPrefixMemory,
    scratch: u64,
    extra: u64,
) -> Option<(u64, u64)> {
    let available = candidate
        .available?
        .checked_add(candidate.build.layout.headroom_bytes)?;
    let partition = memory
        .yielded
        .checked_add(scratch)?
        .checked_add(extra)?
        .checked_add(crate::query::spill::grace_resident_bound())?;
    if partition > available {
        return None;
    }
    // Probe encoding has one codec lease after the W producer yields. Leaf
    // tables load after that producer drains; outer/shared state survives.
    let headroom = scratch
        .checked_add(4096)?
        .saturating_sub(candidate.build.layout.headroom_bytes);
    let outside = memory.drained.checked_add(extra)?.checked_add(headroom)?;
    let target = candidate.resident_target(outside, true);
    #[cfg(test)]
    crate::query::plan::budget::trace::event(format_args!(
        "S8 Grace prefix yielded={} drained={} codec={scratch} partition_peak={partition} available={available} leaf_outside={outside} target={target}",
        memory.yielded, memory.drained
    ));
    (target > 0).then_some((target, partition))
}

fn pair_upper(step: &MatchStep, estimates: &JoinEstimates, keys: &[KeyScopes]) -> Option<u64> {
    if step.parts.len() != 2 || keys.len() != 1 {
        return None;
    }
    let mut upper = None;
    for (part, other) in [(0, 1), (1, 0)] {
        let unique = [&keys[0].left, &keys[0].right]
            .iter()
            .any(|side| side[part].is_some_and(|key| key.unique));
        if unique {
            let bound = estimates.parts[other].output?.upper;
            upper = Some(upper.map_or(bound, |old: u64| old.min(bound)));
        }
    }
    upper
}

pub(super) fn label_slots(step: &MatchStep) -> usize {
    step.parts.iter().fold(0usize, |count, part| {
        count
            .saturating_add(part.start.label_names.len())
            .saturating_add(part.end.as_ref().map_or(0, |node| node.label_names.len()))
    })
}

pub(super) fn greedy_candidates(parts: usize, analysis: &JoinAnalysis) -> usize {
    parts.saturating_mul(
        parts
            .saturating_mul(2)
            .saturating_add(analysis.templates.len())
            .saturating_add(hash_alternatives(analysis)),
    )
}

fn hash_alternatives(analysis: &JoinAnalysis) -> usize {
    analysis.hashes.iter().fold(0usize, |count, hash| {
        count.saturating_add(
            (hash.left.providers.0.count_ones() as usize)
                .saturating_add(hash.right.providers.0.count_ones() as usize)
                .saturating_mul(2),
        )
    })
}

pub(super) fn greedy_storage(
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
        let Some(mut advice) = key_advice(step, key, index, *sample) else {
            continue;
        };
        advice.unique = super::key_facts::unique(index, context.schema);
        if advice.unique {
            advice.ndistinct = CardinalityEstimate::exact(advice.indexed_owners);
            advice.max_frequency = CardinalityEstimate::exact(u64::from(advice.indexed_owners > 0));
            advice.max_observed_frequency = u64::from(advice.indexed_owners > 0);
        }
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
        // Unknown String heap retains the old conservative scratch-based
        // memory proxy. Compact identity JSON is not a String-width proof.
        .unwrap_or_else(|| rows.saturating_mul(candidate.build.layout.encoding_scratch_bytes));
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
