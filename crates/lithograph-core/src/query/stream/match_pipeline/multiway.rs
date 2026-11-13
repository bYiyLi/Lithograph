//! Multiway MATCH output and its existing root node cursor accounting.

use super::*;

impl StepCursor {
    pub(super) fn park_multiway_workspace(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self
            .step
            .join
            .as_ref()
            .is_none_or(|plan| plan.multiway.is_none())
        {
            return Ok(());
        }
        for frame in &mut self.stack {
            control.work(1)?;
            if let MatchFrame::Multiway(producer) = frame {
                producer.park_workspace()?;
            }
        }
        Ok(())
    }

    pub(crate) fn planning_outer_memory_bytes(
        step: &MatchStep,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<u64> {
        let mut bytes = crate::query::resources::allocation_bytes::<MatchFrame>(step.parts.len())?;
        for part in &step.parts {
            bytes = crate::query::resources::checked_add(
                bytes,
                crate::query::plan::part_seek_bytes(part, control)?,
            )?;
        }
        Ok(bytes)
    }

    /// Boxed root cursor, its original 256-row Part pages, and its retained
    /// output are live while the component feeds a following Hash build.
    pub(crate) fn planning_node_memory_bytes(
        spec: &crate::query::plan::NodeSpec,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<u64> {
        use crate::query::resources::{allocation_bytes, checked_add};
        let seek = spec
            .index_seek
            .as_ref()
            .map(|seek| crate::query::schema::seek_bytes(seek, control))
            .transpose()?
            .unwrap_or(0);
        let empty = allocation_bytes::<BindingRow>(1)?;
        let step = checked_add(empty, allocation_bytes::<MatchFrame>(1)?)?;
        let part = checked_add(empty, PartCursor::page_memory_bytes()?)?;
        let part = checked_add(part, std::mem::size_of::<PartCursor>() as u64)?;
        let bytes = checked_add(step, part)?;
        let bytes = checked_add(bytes, crate::query::resources::checked_mul(seek, 2)?)?;
        let bytes = checked_add(bytes, super::output::node_output_row_bytes(spec)?)?;
        checked_add(bytes, std::mem::size_of::<StepCursor>() as u64)
    }

    pub(super) fn initialize_multiway_frame(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let Some(producer) = self.multiway.take() else {
            return Ok(false);
        };
        let replaced = prefix_length(&self.step)?;
        control.work(replaced as u64)?;
        // Depth continues to address the original JoinPlan stages and Hash
        // owners. The consumed slots stand for component Parts replaced by
        // this one real producer, just as a drained Grace prefix does.
        self.stack
            .resize_with(replaced - 1, || MatchFrame::Consumed);
        self.stack.push(MatchFrame::Multiway(producer));
        Ok(true)
    }

    pub(super) fn accept_multiway_prefix(
        &self,
        row: &BindingRow,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        // This frame's width was verified when it entered the original stack.
        let replaced = self.stack.len();
        if replaced == self.step.parts.len() {
            // Preserve the whole-MATCH path: its sole qualification boundary
            // remains the complete original WHERE at final output.
            return Ok(true);
        }
        let plan = self.step.join.as_ref().ok_or_else(|| {
            QueryError::internal("multiway component lost its original Join plan")
        })?;
        for depth in 0..replaced {
            control.work(1)?;
            let stage = &plan.stages[depth];
            if stage.predicates_after.is_empty() {
                continue;
            }
            if self.join_prefix_rejects(depth, row, scope.0, scope.1, params, control)? {
                return Ok(false);
            }
            let slot = metrics
                .match_profile_slots(plan.clause_start, stage.part)
                .and_then(|slots| slots.slot(ProfileRole::EarlyFilter));
            metrics.record_profile_rows(slot, 1);
        }
        Ok(true)
    }

    /// A complete, qualified row has the same output bookkeeping on both paths.
    pub(super) fn finish_match_row(&mut self, row: &mut BindingRow, metrics: &mut QueryMetrics) {
        if self.step.predicate.is_some() {
            metrics.record_profile_rows(metrics.match_residual_slot(self.step.clause_start), 1);
        }
        self.matched = true;
        self.restore_declared_order(row);
    }

    /// The multiway owner uses this only for its plain node-only root cursor.
    /// All these leases remain live while one root identity feeds intersections.
    pub(crate) fn node_scan_memory_bytes(&self) -> u64 {
        let mut bytes = self
            ._memory
            .bytes()
            .saturating_add(self._seek_memory.bytes());
        bytes = bytes.saturating_add(
            self.output_memory
                .as_ref()
                .map_or(0, MemoryReservation::bytes),
        );
        if let Some((_, memory)) = &self.node_ids {
            bytes = bytes.saturating_add(memory.bytes());
        }
        for frame in &self.stack {
            if let MatchFrame::Part(frame) = frame {
                bytes = bytes.saturating_add(frame._memory.bytes());
                bytes = bytes.saturating_add(
                    frame
                        .output_memory
                        .as_ref()
                        .map_or(0, MemoryReservation::bytes),
                );
                bytes = bytes.saturating_add(
                    frame
                        .probe_memory
                        .as_ref()
                        .map_or(0, MemoryReservation::bytes),
                );
                bytes = bytes.saturating_add(
                    frame
                        ._finite_memory
                        .as_ref()
                        .map_or(0, MemoryReservation::bytes),
                );
            }
        }
        bytes
    }
}

fn prefix_length(step: &MatchStep) -> QueryResult<usize> {
    let join = step
        .join
        .as_ref()
        .ok_or_else(|| QueryError::internal("multiway component has no Join plan"))?;
    let component = join
        .multiway
        .as_ref()
        .ok_or_else(|| QueryError::internal("multiway component description is missing"))?;
    let count = component.edges.len();
    if count == 0 || count > join.stages.len() || step.parts.len() != join.stages.len() {
        return Err(QueryError::internal(
            "multiway component has an invalid prefix width",
        ));
    }
    let described = component
        .edges
        .iter()
        .fold(0u128, |mask, edge| mask | (1u128 << edge.part));
    let scheduled = join
        .stages
        .iter()
        .take(count)
        .fold(0u128, |mask, stage| mask | (1u128 << stage.part));
    if described.count_ones() as usize != count || described != scheduled {
        return Err(QueryError::internal(
            "multiway component is not the original Join prefix",
        ));
    }
    Ok(count)
}
