//! A complete INTEGER build-key set can replace only an unread Probe tail.
//! The first physical page and its remaining rows keep the original cursor.

use super::*;
use crate::query::plan::join::{ProbeAccess, StageAccess};
use crate::query::schema::{BoundStandardIndexRead, StandardIndexCursor};
use crate::query::stream::hash_metrics::JoinDynamicFilterRuntimeMetrics;

const MAX_KEYS: usize = crate::query::plan::join::DYNAMIC_FILTER_MAX_KEYS;
const BIND_HEADROOM: u64 = 64 * 1024;
const MAX_PROBES: usize = 16;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(super) struct KeyCollector {
    keys: Vec<IntegerKey>,
    buckets: Vec<usize>,
    memory: MemoryReservation,
    peak_bytes: u64,
    last_peak_bytes: u64,
    probes: u64,
}

#[derive(Debug, Clone, Copy)]
struct IntegerKey {
    value: i64,
    hash: u64,
}

enum Position {
    Existing,
    Vacant(usize),
    Abandon,
}

impl KeyCollector {
    pub(super) fn new(control: &ExecutionControl<'_>) -> QueryResult<Option<Self>> {
        Ok(advisory_reserve(0, control)?.map(|memory| Self {
            keys: Vec::new(),
            buckets: Vec::new(),
            memory,
            peak_bytes: 0,
            last_peak_bytes: 0,
            probes: 0,
        }))
    }

    pub(super) fn observe(
        &mut self,
        key: &EvaluatedHashKey,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        self.last_peak_bytes = self.memory.bytes();
        let key::HashKeyValue::Scalar(Value::Integer(value)) = &key.value else {
            return Ok(false);
        };
        let hash = u64::from_le_bytes(key.digest[..8].try_into().map_err(|_| {
            QueryError::internal("Dynamic Filter received a truncated Hash digest")
        })?);
        if !self.buckets.is_empty() {
            match self.position(hash, *value, control)? {
                Position::Existing => return Ok(true),
                Position::Abandon => return Ok(false),
                Position::Vacant(_) => {}
            }
        }
        if self.keys.len() == MAX_KEYS || !self.grow(control)? {
            return Ok(false);
        }
        match self.position(hash, *value, control)? {
            Position::Existing => Ok(true),
            Position::Abandon => Ok(false),
            Position::Vacant(bucket) => {
                self.keys.push(IntegerKey {
                    value: *value,
                    hash,
                });
                self.buckets[bucket] = self.keys.len();
                Ok(true)
            }
        }
    }

    fn position(
        &mut self,
        hash: u64,
        value: i64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Position> {
        let mask = self.buckets.len() - 1;
        for offset in 0..MAX_PROBES {
            control.work(1)?;
            self.probes = self.probes.saturating_add(1);
            let bucket = (hash as usize).wrapping_add(offset) & mask;
            let index = self.buckets[bucket];
            if index == 0 {
                return Ok(Position::Vacant(bucket));
            }
            if self.keys[index - 1].value == value {
                return Ok(Position::Existing);
            }
        }
        Ok(Position::Abandon)
    }

    fn grow(&mut self, control: &ExecutionControl<'_>) -> QueryResult<bool> {
        if self.keys.len() < self.keys.capacity() {
            return Ok(true);
        }
        let capacity = self.keys.capacity().saturating_mul(2).clamp(8, MAX_KEYS);
        let bytes = checked_add(
            allocation_bytes::<IntegerKey>(capacity)?,
            allocation_bytes::<usize>(capacity * 2)?,
        )?;
        let Some(extra) = advisory_reserve(bytes, control)? else {
            return Ok(false);
        };
        self.peak_bytes = self
            .peak_bytes
            .max(checked_add(self.memory.bytes(), bytes)?);
        self.last_peak_bytes = checked_add(self.memory.bytes(), bytes)?;
        // Pay both live buffers before replacing the old one.
        let mut keys = Vec::with_capacity(capacity);
        let mut buckets = vec![0; capacity * 2];
        control.bytes(allocation_bytes::<IntegerKey>(self.keys.len())?)?;
        keys.extend_from_slice(&self.keys);
        for (index, key) in keys.iter().enumerate() {
            if !place_key(&mut buckets, index + 1, key.hash, &mut self.probes, control)? {
                return Ok(false);
            }
        }
        self.keys = keys;
        self.buckets = buckets;
        self.memory = extra;
        Ok(true)
    }

    pub(super) fn owned_bytes(&self) -> u64 {
        self.memory.bytes()
    }

    pub(super) fn last_peak_bytes(&self) -> u64 {
        self.last_peak_bytes
    }
}

fn place_key(
    buckets: &mut [usize],
    index: usize,
    hash: u64,
    probes: &mut u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let mask = buckets.len() - 1;
    for offset in 0..MAX_PROBES {
        control.work(1)?;
        *probes = probes.saturating_add(1);
        let bucket = (hash as usize).wrapping_add(offset) & mask;
        if buckets[bucket] == 0 {
            buckets[bucket] = index;
            return Ok(true);
        }
    }
    Ok(false)
}

/// Only failures of this exact local allocation are advisory. SQLite, binding,
/// cancellation and source-validation errors are never caught as fallback.
fn advisory_reserve(
    bytes: u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<MemoryReservation>> {
    control.check()?;
    match control.resources().reserve(bytes) {
        Ok(memory) => Ok(Some(memory)),
        Err(error) if error.kind == crate::query::QueryErrorKind::Resource => Ok(None),
        Err(error) => Err(error),
    }
}

#[derive(Debug)]
pub(in crate::query::stream::match_pipeline) struct ProbeFilter {
    keys: KeyCollector,
    bound: BoundStandardIndexRead,
    frontier: i64,
    consumed: bool,
    slot: Option<usize>,
    stats: JoinDynamicFilterRuntimeMetrics,
}

impl StepCursor {
    pub(in crate::query::stream::match_pipeline) fn install_dynamic_filter(
        &mut self,
        depth: usize,
        state: &mut HashBuildState,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let Some(keys) = state.filter.take() else {
            return Ok(());
        };
        if keys.keys.is_empty() {
            return Ok(());
        }
        let Some(plan) = &self.step.join else {
            return Ok(());
        };
        let StageAccess::Hash {
            dynamic_filter: Some(template),
            ..
        } = plan.stages[depth].access
        else {
            return Ok(());
        };
        let Some(cursor::MatchFrame::Part(frame)) = self.stack.last_mut() else {
            return Ok(());
        };
        // The costed continuation is one immutable, independent singleton
        // producer; never install it on a later page or another access path.
        if depth != 1
            || frame.start_done
            || frame.start_buffer.is_empty()
            || frame.start_buffer.len() > PIPELINE_BATCH
            || frame.part.relationship.is_some()
            || frame.part.start.index_seek.is_some()
            || frame.part.start.id_seek.is_some()
            || frame.dynamic_filter.is_some()
        {
            return Ok(());
        }
        let ProbeAccess::IndexEquality {
            index_name,
            definition_hash,
            ..
        } = &plan.templates[template].access
        else {
            return Ok(());
        };
        let Some(bound) = crate::query::schema::try_bind_dynamic_filter_index(
            scope.0,
            index_name,
            *definition_hash,
            control,
        )?
        else {
            return Ok(());
        };
        let frontier = *frame.start_buffer.last().ok_or_else(|| {
            QueryError::internal("Dynamic Filter lost its already-read Probe page")
        })?;
        let slot = hash_slot(metrics, plan, depth);
        let stats = JoinDynamicFilterRuntimeMetrics {
            build_rows: state.stats.build_rows,
            distinct_keys: keys.keys.len() as u64,
            keyset_peak_bytes: keys.peak_bytes,
            collector_probes: keys.probes,
            prefix_rows_read: frame.start_buffer.len() as u64,
            ..JoinDynamicFilterRuntimeMetrics::default()
        };
        metrics.set_dynamic_filter_profile(slot, stats);
        frame.dynamic_filter = Some(ProbeFilter {
            keys,
            bound,
            frontier,
            consumed: false,
            slot,
            stats,
        });
        Ok(())
    }
}

impl PartCursor {
    pub(in crate::query::stream::match_pipeline) fn load_dynamic_filter_page(
        &mut self,
        snapshot: &Snapshot<'_>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let Some(filter) = &mut self.dynamic_filter else {
            return Ok(false);
        };
        if filter.consumed {
            self.start_done = true;
            return Ok(true);
        }
        // A Core caller may retain another lease while this cursor is paused
        // after its first output batch. Recheck the optional whole-tail grant
        // at consumption, including the bound INTEGER conversion/page scratch.
        let owner_bytes = allocation_bytes::<i64>(filter.keys.keys.len())?;
        if control.resources().limit().is_some_and(|limit| {
            limit.saturating_sub(control.resources().used())
                < owner_bytes.saturating_add(BIND_HEADROOM)
        }) {
            self.dynamic_filter = None;
            return Ok(false);
        }
        let Some(memory) = advisory_reserve(owner_bytes, control)? else {
            // No tail has been read or skipped yet: retain the original
            // Label continuation and discard the entire optional filter.
            self.dynamic_filter = None;
            return Ok(false);
        };
        let mut owners = Vec::with_capacity(filter.keys.keys.len());
        for key in &filter.keys.keys {
            control.work(1)?;
            let page = crate::query::schema::scan_bound_index_page(
                snapshot,
                &filter.bound,
                &Value::Integer(key.value),
                Some(&StandardIndexCursor::Owner(filter.frontier)),
                2,
                control,
            )?;
            filter.stats.equality_seeks = filter.stats.equality_seeks.saturating_add(1);
            metrics.set_dynamic_filter_profile(filter.slot, filter.stats);
            metrics.record_db_hits(page.items.len() as u64);
            // The pinned single-column Key proves at most one owner. Do not
            // use a partial page if that capability no longer holds.
            if page.items.len() > 1 || page.next_cursor.is_some() {
                self.dynamic_filter = None;
                return Ok(false);
            }
            owners.extend(page.items);
        }
        control.work(
            owners
                .len()
                .saturating_mul(owners.len().max(1).ilog2() as usize + 1) as u64,
        )?;
        owners.sort_unstable();
        owners.dedup();
        filter.stats.tail_owners = owners.len() as u64;
        filter.stats.applied = true;
        metrics.set_dynamic_filter_profile(filter.slot, filter.stats);
        filter.keys.keys = Vec::new();
        filter.keys.buckets = Vec::new();
        filter.keys.memory = memory;
        filter.consumed = true;
        self.start_buffer = owners;
        self.start_index = 0;
        // A Range owner still passes the original label/view/access checks;
        // the already-read Label page's known-label proof does not transfer.
        self.start_known_label = None;
        self.start_done = true;
        Ok(true)
    }
}
