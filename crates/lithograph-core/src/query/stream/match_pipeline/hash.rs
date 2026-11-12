//! A clause-owned singleton build and resumable prefix probe. The ordinary
//! MATCH cursor remains responsible for suffixes, residuals and publication.

use super::*;
use crate::query::HashJoinRuntimeMetrics;
use crate::query::plan::join::{HashEquality, HashFamily, HashOperandSide, JoinPlan};
use crate::query::resources::{QueryResources, allocation_bytes, checked_add, checked_mul};
use crate::query::spill::{GracePartitions, HashSide, SpillReadLimit};

mod build;
mod grace;
mod key;
mod probe;
mod table;

use key::{EvaluatedHashKey, extract_hash_key, hash_keys_equal};
pub(super) use probe::HashProbe;
use table::{HashEntry, HashTable};

/// An implementation spill target, never a public single-value restriction.
pub(crate) const HASH_RESIDENT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone, Copy)]
pub(crate) struct HashLayoutFacts {
    pub(crate) entry_inline_bytes: u64,
    pub(crate) bucket_bytes: u64,
    pub(crate) binding_bytes: u64,
    pub(crate) key_heap_bytes: Option<u64>,
    pub(crate) encoded_row_bytes: u64,
    pub(crate) headroom_bytes: u64,
    pub(crate) soft_cap: u64,
}

pub(crate) fn hash_layout(
    part: &PatternPart,
    equality: &HashEquality,
    _inner: HashOperandSide,
) -> QueryResult<HashLayoutFacts> {
    let bytes = pattern_binding_bytes(part)?;
    let encoded = crate::query::spill::serialization_bytes(bytes)?;
    Ok(HashLayoutFacts {
        entry_inline_bytes: std::mem::size_of::<HashEntry>() as u64,
        bucket_bytes: std::mem::size_of::<Option<usize>>() as u64,
        binding_bytes: bytes.saturating_sub(std::mem::size_of::<BindingRow>() as u64),
        key_heap_bytes: (equality.family != HashFamily::String).then_some(0),
        encoded_row_bytes: encoded,
        headroom_bytes: checked_add(checked_mul(encoded, 2)?, 4096)?,
        soft_cap: HASH_RESIDENT_BYTES,
    })
}

fn pattern_binding_bytes(part: &PatternPart) -> QueryResult<u64> {
    let variables = super::output::binding_names(part);
    let mut bytes = allocation_bytes::<BindingRow>(1)?;
    let mut count = 0;
    for (position, name) in variables.iter().enumerate() {
        let Some(name) = name else { continue };
        if variables[..position].contains(&Some(name)) {
            continue;
        }
        count += 1;
        bytes = checked_add(
            bytes,
            crate::query::resources::map_entry_bytes::<BindingValue>()?,
        )?;
        bytes = checked_add(bytes, checked_mul(name.len() as u64, 2)?)?;
    }
    if count > 0 {
        bytes = checked_add(bytes, allocation_bytes::<String>(count.max(4))?)?;
    }
    if part.relationship.is_some() {
        bytes = checked_add(bytes, crate::query::resources::map_entry_bytes::<i64>()?)?;
    }
    Ok(bytes)
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
pub(super) struct HashBuildState {
    pub(super) storage: BuildStorage,
    built: bool,
    stats: HashJoinRuntimeMetrics,
    pub(super) probe_metadata: u64,
}

impl HashBuildState {
    pub(super) fn new() -> Self {
        Self {
            storage: BuildStorage::Empty,
            built: false,
            stats: HashJoinRuntimeMetrics::default(),
            probe_metadata: 0,
        }
    }

    pub(super) fn finish(
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
    metrics
        .match_profile_slots(plan.clause_start, plan.stages[depth].part)?
        .slot(ProfileRole::Join)
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
