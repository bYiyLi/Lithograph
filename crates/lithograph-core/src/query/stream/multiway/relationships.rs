//! Enumerate exact endpoint relationship domains without materializing products.

use super::adjacency::AdjacentList;
use super::*;
use crate::storage::RelationshipRecord;

#[derive(Debug)]
pub(super) struct RelationshipProduct {
    scans: Vec<AdjacentList>,
    targets: Vec<i64>,
    selected: Vec<RelationshipRecord>,
    depth: usize,
    done: bool,
    memory: MemoryReservation,
}

impl RelationshipProduct {
    pub(super) fn new(
        plan: &MultiwayPlan,
        nodes: &[i64],
        stats: &mut MultiwayRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let length = plan.edges.len();
        let bytes = checked_add(
            allocation_bytes::<AdjacentList>(length)?,
            allocation_bytes::<i64>(length)?,
        )?;
        let memory = control.reserve(checked_add(
            bytes,
            allocation_bytes::<RelationshipRecord>(length)?,
        )?)?;
        let mut scans = Vec::with_capacity(length);
        let mut targets = Vec::with_capacity(length);
        for edge in &plan.edges {
            control.work(1)?;
            scans.push(AdjacentList::new(
                nodes[edge.source],
                edge.type_id,
                true,
                stats,
                control,
            )?);
            targets.push(nodes[edge.target]);
        }
        Ok(Self {
            scans,
            targets,
            selected: Vec::with_capacity(length),
            depth: 0,
            done: false,
            memory,
        })
    }

    pub(super) fn next(
        &mut self,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        stats: &mut MultiwayRuntimeMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        while !self.done {
            control.work(1)?;
            self.selected.truncate(self.depth);
            let next = self.scans[self.depth].next_relationship_at_neighbor(
                self.targets[self.depth],
                scope.0,
                metrics,
                stats,
                control,
            )?;
            let Some(relationship) = next else {
                if self.depth == 0 {
                    self.done = true;
                    return Ok(false);
                }
                self.depth -= 1;
                self.selected.truncate(self.depth);
                continue;
            };
            stats.relationship_candidates = stats.relationship_candidates.saturating_add(1);
            if !scope
                .1
                .visible_relationship_controlled(scope.0, relationship, control)?
                || self
                    .selected
                    .iter()
                    .any(|previous| previous.id == relationship.id)
            {
                continue;
            }
            self.selected.push(relationship);
            if self.depth + 1 == self.scans.len() {
                // Keep this domain positioned immediately after the chosen
                // RID; the next call continues this last domain's bag.
                return Ok(true);
            }
            self.depth += 1;
            self.scans[self.depth].reset(stats);
        }
        Ok(false)
    }

    pub(super) fn selected(&self) -> impl Iterator<Item = RelationshipRecord> + '_ {
        self.selected.iter().copied()
    }

    pub(super) fn retained_bytes(&self) -> u64 {
        self.memory.bytes().saturating_add(
            self.scans
                .iter()
                .map(AdjacentList::retained_bytes)
                .sum::<u64>(),
        )
    }
}
