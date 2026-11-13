//! A bounded typed neighbor stream over the Snapshot's real ordered access.

use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};
use crate::query::{
    ExecutionControl, MemoryReservation, MultiwayRuntimeMetrics, QueryMetrics, QueryResult,
};
use crate::storage::{AdjacencyCursor, RelationshipRecord, RelationshipTypeId, Snapshot};

const PAGE_ROWS: usize = 32;

/// A neighbor candidate remains at `index` until the caller seeks beyond it.
/// The same bounded stream also enumerates the Relationship bag at one fixed
/// neighbor; a caller uses a separate stream for that consuming mode.
#[derive(Debug)]
pub(super) struct AdjacentList {
    node: i64,
    type_id: RelationshipTypeId,
    outgoing: bool,
    cursor: Option<AdjacencyCursor>,
    items: Vec<RelationshipRecord>,
    index: usize,
    done: bool,
    memory: MemoryReservation,
}

impl AdjacentList {
    pub(super) fn new(
        node: i64,
        type_id: RelationshipTypeId,
        outgoing: bool,
        stats: &mut MultiwayRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        control.check()?;
        let memory = control.reserve(page_retained_bytes()?)?;
        stats.input_lists = stats.input_lists.saturating_add(1);
        Ok(Self {
            node,
            type_id,
            outgoing,
            cursor: None,
            items: Vec::new(),
            index: 0,
            done: false,
            memory,
        })
    }

    pub(super) fn retained_bytes(&self) -> u64 {
        self.memory.bytes()
    }

    /// Restarts this same typed endpoint domain for a new bag-product prefix.
    /// Its checked page capacity remains owned.
    pub(super) fn reset(&mut self, stats: &mut MultiwayRuntimeMetrics) {
        self.items = Vec::new();
        self.cursor = None;
        self.index = 0;
        self.done = false;
        stats.input_lists = stats.input_lists.saturating_add(1);
    }

    pub(super) fn advance_at_least(
        &mut self,
        target: i64,
        snapshot: &Snapshot<'_>,
        metrics: &mut QueryMetrics,
        stats: &mut MultiwayRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<i64>> {
        loop {
            control.work(1)?;
            while let Some(relationship) = self.items.get(self.index) {
                control.work(1)?;
                stats.intersection_steps = stats.intersection_steps.saturating_add(1);
                let neighbor = self.neighbor(*relationship);
                if neighbor >= target {
                    return Ok(Some(neighbor));
                }
                self.index += 1;
            }
            if self.done {
                return Ok(None);
            }
            self.load_page(target, snapshot, metrics, stats, control)?;
        }
    }

    pub(super) fn next_relationship_at_neighbor(
        &mut self,
        target: i64,
        snapshot: &Snapshot<'_>,
        metrics: &mut QueryMetrics,
        stats: &mut MultiwayRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<RelationshipRecord>> {
        if self.advance_at_least(target, snapshot, metrics, stats, control)? != Some(target) {
            return Ok(None);
        }
        control.work(1)?;
        let relationship = self.items[self.index];
        self.index += 1;
        Ok(Some(relationship))
    }

    fn neighbor(&self, relationship: RelationshipRecord) -> i64 {
        if self.outgoing {
            relationship.target
        } else {
            relationship.source
        }
    }

    fn load_page(
        &mut self,
        minimum: i64,
        snapshot: &Snapshot<'_>,
        metrics: &mut QueryMetrics,
        stats: &mut MultiwayRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        control.check()?;
        // Release the previous allocation before the merge allocates its next
        // bounded workspace. Its opaque continuation already owns the key.
        self.items = Vec::new();
        self.index = 0;
        // The producer holds one shared, live workspace reservation covering
        // base + overlay pages and at most two pages of B-tree candidates.
        // Page loads are synchronous; the returned page retains this list's
        // separate reservation throughout the following intersection work.
        stats.seek_operations = stats.seek_operations.saturating_add(1);
        let page = if self.outgoing {
            snapshot.scan_outgoing_from_neighbor_page(
                self.node,
                self.type_id,
                minimum,
                self.cursor,
                PAGE_ROWS,
            )?
        } else {
            snapshot.scan_incoming_from_neighbor_page(
                self.node,
                self.type_id,
                minimum,
                self.cursor,
                PAGE_ROWS,
            )?
        };
        control.check()?;
        metrics.record_db_hits(page.items.len() as u64);
        stats.adjacency_rows = stats.adjacency_rows.saturating_add(page.items.len() as u64);
        self.cursor = page.next_cursor;
        self.done = self.cursor.is_none();
        self.items = page.items;
        Ok(())
    }
}

pub(super) fn page_retained_bytes() -> QueryResult<u64> {
    checked_add(
        allocation_bytes::<RelationshipRecord>(PAGE_ROWS)?,
        std::mem::size_of::<AdjacentList>() as u64,
    )
}

pub(super) fn scratch_workspace_bytes() -> QueryResult<u64> {
    let base = allocation_bytes::<RelationshipRecord>(PAGE_ROWS)?;
    // This is an upper bound for (AdjacencyKey, RelationshipRecord, DeltaOp).
    let overlay = allocation_bytes::<[i64; 8]>(PAGE_ROWS)?;
    // AdjacencyKey and String have the same three-word inline representation;
    // map_entry_bytes additionally allows sparse B-tree nodes and headers.
    let candidates = checked_mul(
        map_entry_bytes::<Option<RelationshipRecord>>()?,
        (PAGE_ROWS * 2) as u64,
    )?;
    checked_add(checked_add(base, overlay)?, candidates)
}
