//! A clause-owned singleton build and resumable prefix probe. The ordinary
//! MATCH cursor remains responsible for suffixes, residuals and publication.

use super::*;
use crate::query::HashJoinRuntimeMetrics;
use crate::query::plan::join::{HashEquality, HashFamily, HashOperandSide, JoinPlan};
use crate::query::resources::{QueryResources, allocation_bytes, checked_add, checked_mul};
use crate::query::spill::{GracePartitions, HashSide, SpillReadLimit};

mod build;
pub(super) mod dynamic_filter;
mod grace;
mod key;
mod probe;
mod table;
#[cfg(test)]
mod tests;

use key::{EvaluatedHashKey, extract_hash_key, extract_hash_key_admitted, hash_keys_equal};
pub(in crate::query::stream) use probe::HashProbe;
use table::{HashEntry, HashTable};

/// An implementation spill target, never a public single-value restriction.
pub(crate) const HASH_RESIDENT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct HashLayoutFacts {
    pub(crate) entry_inline_bytes: u64,
    pub(crate) bucket_bytes: u64,
    pub(crate) binding_bytes: u64,
    pub(crate) key_heap_bytes: Option<u64>,
    /// Codec payload, excluding DOM/String construction scratch. Unknown
    /// binding families cannot be assigned a fictitious transport width.
    pub(crate) encoded_row_bytes: Option<u64>,
    pub(crate) encoding_scratch_bytes: u64,
    pub(crate) headroom_bytes: u64,
    pub(crate) soft_cap: u64,
}

pub(crate) fn hash_layout(
    part: &PatternPart,
    equality: &HashEquality,
    _inner: HashOperandSide,
) -> QueryResult<HashLayoutFacts> {
    let bytes = pattern_binding_bytes(part)?;
    let scratch = crate::query::spill::serialization_bytes(bytes)?;
    let encoded = if part.path_variable.is_some() {
        None
    } else {
        Some(crate::query::spill::identity_binding_encoded_upper(
            [
                part.start.variable.as_deref(),
                part.relationship
                    .as_ref()
                    .and_then(|rel| rel.variable.as_deref()),
                part.end.as_ref().and_then(|node| node.variable.as_deref()),
            ],
            usize::from(part.relationship.is_some()),
        )?)
    };
    Ok(HashLayoutFacts {
        entry_inline_bytes: std::mem::size_of::<HashEntry>() as u64,
        bucket_bytes: std::mem::size_of::<Option<usize>>() as u64,
        binding_bytes: bytes.saturating_sub(std::mem::size_of::<BindingRow>() as u64),
        key_heap_bytes: (equality.family != HashFamily::String).then_some(0),
        encoded_row_bytes: encoded,
        encoding_scratch_bytes: scratch,
        headroom_bytes: checked_add(checked_mul(scratch, 2)?, 4096)?,
        soft_cap: HASH_RESIDENT_BYTES,
    })
}

fn pattern_binding_bytes(part: &PatternPart) -> QueryResult<u64> {
    let variables = super::output::binding_names(part);
    let mut count = 0;
    let mut name_bytes = 0;
    for (position, name) in variables.iter().enumerate() {
        let Some(name) = name else { continue };
        if variables[..position].contains(&Some(name)) {
            continue;
        }
        count += 1;
        name_bytes = checked_add(name_bytes, name.len() as u64)?;
    }
    identity_binding_row_bytes(
        count,
        name_bytes,
        usize::from(part.relationship.is_some()),
        if count > 0 { count.max(4) } else { 0 },
    )
}

/// Retained identity-only BindingRow shape. Callers prove unique named
/// variables, separately count anonymous Relationships in the used set, and
/// bound the actual Vec capacity of the row's declaration order. No scalar,
/// Path or inherited LOAD CSV payload width is inferred by this helper.
pub(crate) fn identity_binding_row_bytes(
    variables: usize,
    name_bytes: u64,
    relationships: usize,
    order_capacity: usize,
) -> QueryResult<u64> {
    let bytes = checked_add(
        allocation_bytes::<BindingRow>(1)?,
        checked_mul(
            crate::query::resources::map_entry_bytes::<BindingValue>()?,
            variables as u64,
        )?,
    )?;
    let bytes = checked_add(bytes, checked_mul(name_bytes, 2)?)?;
    let bytes = checked_add(bytes, allocation_bytes::<String>(order_capacity)?)?;
    checked_add(
        bytes,
        checked_mul(
            crate::query::resources::map_entry_bytes::<i64>()?,
            relationships as u64,
        )?,
    )
}

/// `owned` and `incoming` already occur in used; remove them only because the
/// projected table total below includes those exact same live claims.
pub(crate) fn hash_resident_target(
    resources: &QueryResources,
    owned: u64,
    incoming: u64,
    headroom: u64,
) -> u64 {
    let outside = resources
        .used()
        .saturating_sub(owned)
        .saturating_sub(incoming);
    resources.limit().map_or(HASH_RESIDENT_BYTES, |limit| {
        HASH_RESIDENT_BYTES.min(limit.saturating_sub(outside).saturating_sub(headroom))
    })
}

/// Heap payload excludes the inline row and scalar key already paid by entries.
pub(crate) fn hash_table_peak_bytes(rows: u64, facts: &HashLayoutFacts) -> Option<u64> {
    table::estimate_peak(
        rows,
        facts.binding_bytes.checked_add(facts.key_heap_bytes?)?,
    )
}

#[derive(Debug)]
pub(super) enum BuildStorage {
    Empty,
    Memory(HashTable),
    Spill(Box<GracePartitions>),
}

#[derive(Debug)]
pub(in crate::query::stream) struct HashBuildState {
    pub(super) storage: BuildStorage,
    built: bool,
    stats: HashJoinRuntimeMetrics,
    pub(super) probe_metadata: u64,
    filter: Option<dynamic_filter::KeyCollector>,
}

impl HashBuildState {
    fn admit_key_allocation(
        &mut self,
        bytes: u64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        control.check()?;
        let Some(limit) = control.resources().limit() else {
            return Ok(());
        };
        if bytes <= limit.saturating_sub(control.resources().used()) {
            return Ok(());
        }
        // Optional advice cannot be the reason the mandatory key cannot fit.
        self.filter = None;
        let available = limit.saturating_sub(control.resources().used());
        let BuildStorage::Memory(table) = &self.storage else {
            return Ok(());
        };
        let recovered = available
            .saturating_add(table.owned_bytes())
            .saturating_sub(crate::query::spill::grace_resident_bound());
        if bytes > available && table.len() > 0 && bytes <= recovered {
            self.spill_build(control)?;
        }
        Ok(())
    }

    fn storage_bytes(&self) -> QueryResult<u64> {
        match &self.storage {
            BuildStorage::Empty => Ok(0),
            BuildStorage::Memory(table) => Ok(table.owned_bytes()),
            BuildStorage::Spill(store) => store.resident_bytes(),
        }
    }

    pub(in crate::query::stream) fn new() -> Self {
        Self {
            storage: BuildStorage::Empty,
            built: false,
            stats: HashJoinRuntimeMetrics::default(),
            probe_metadata: 0,
            filter: None,
        }
    }

    pub(in crate::query::stream) fn is_spilled(&self) -> bool {
        matches!(self.storage, BuildStorage::Spill(_))
    }

    pub(in crate::query::stream) fn runtime_stats(&self) -> HashJoinRuntimeMetrics {
        self.stats
    }

    pub(in crate::query::stream) fn record_probe_metadata(&mut self, probe: &HashProbe) {
        self.probe_metadata = self.probe_metadata.max(probe.metadata_bytes());
    }

    pub(in crate::query::stream) fn finish(
        &mut self,
        metrics: &mut QueryMetrics,
        slot: Option<usize>,
    ) -> QueryResult<()> {
        if let BuildStorage::Spill(store) = &self.storage {
            let counts = store.counters();
            self.record_spill_counts(counts);
        }
        let storage = std::mem::replace(&mut self.storage, BuildStorage::Empty);
        if let BuildStorage::Spill(store) = storage {
            self.record_spill_counts((*store).finish()?);
        }
        self.publish(metrics, slot)
    }

    fn record_spill_counts(&mut self, counts: crate::query::spill::GraceCounters) {
        self.stats.spill_partitions = counts.partitions;
        self.stats.spill_batches = counts
            .read_batches
            .saturating_add(counts.repartition_batches)
            .saturating_add(counts.retired_probe_batches);
        self.stats.spill_vm_steps = counts.sqlite_vm_steps;
        self.stats.spill_encoded_bytes =
            counts.written_bytes[0].saturating_add(counts.written_bytes[1]);
        self.stats.temp_allocated_bytes = counts.temp_logical_bytes_peak;
    }

    pub(super) fn publish(
        &mut self,
        metrics: &mut QueryMetrics,
        slot: Option<usize>,
    ) -> QueryResult<()> {
        self.stats.memory_peak_bytes = self.stats.memory_peak_bytes.max(checked_add(
            self.storage_bytes()?,
            self.filter
                .as_ref()
                .map_or(0, |filter| filter.owned_bytes()),
        )?);
        if let BuildStorage::Spill(store) = &self.storage {
            let counts = store.counters();
            let resident = store.resident_bytes()?;
            self.record_spill_counts(counts);
            self.stats.memory_peak_bytes = self.stats.memory_peak_bytes.max(resident);
        }
        let mut published = self.stats;
        let fixed = checked_add(
            std::mem::size_of::<Option<HashBuildState>>() as u64,
            std::mem::size_of::<super::cursor::HashFrame>() as u64,
        )?;
        published.memory_peak_bytes = checked_add(
            published.memory_peak_bytes,
            checked_add(fixed, self.probe_metadata)?,
        )?;
        metrics.set_hash_profile(slot, published);
        Ok(())
    }
}

pub(super) fn hash_slot(metrics: &QueryMetrics, plan: &JoinPlan, depth: usize) -> Option<usize> {
    let part = plan.stages[depth].part;
    if let Some(slot) = metrics
        .match_profile_slots(plan.clause_start, part)
        .and_then(|slots| slots.slot(ProfileRole::Join))
    {
        return Some(slot);
    }
    let asp = plan.factorized.as_ref()?.asp.as_ref()?;
    if asp.part == part {
        metrics.factorized_slot(plan.clause_start, "ASPJoin")
    } else {
        None
    }
}

fn row_headroom(bytes: u64) -> QueryResult<u64> {
    checked_add(
        checked_mul(crate::query::spill::serialization_bytes(bytes)?, 2)?,
        4096,
    )
}

fn spill_limit(control: &ExecutionControl<'_>) -> SpillReadLimit {
    let available = control
        .resources()
        .limit()
        .map_or(HASH_RESIDENT_BYTES, |limit| {
            limit.saturating_sub(control.resources().used())
        });
    SpillReadLimit {
        rows: PIPELINE_BATCH,
        bytes: (available / 4).clamp(1, 64 * 1024),
    }
}

impl PartCursor {
    pub(super) fn split_output_memory(
        &mut self,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<MemoryReservation> {
        let bytes = expression::binding_row_bytes_controlled(row, Some(control))?;
        self.output_memory
            .as_mut()
            .ok_or_else(|| {
                QueryError::internal("MATCH producer returned a row without its reservation")
            })?
            .split(bytes)
    }
}

impl Drop for HashBuildState {
    fn drop(&mut self) {
        let storage = std::mem::replace(&mut self.storage, BuildStorage::Empty);
        if let BuildStorage::Spill(store) = storage {
            let _ = (*store).abort();
        }
    }
}
