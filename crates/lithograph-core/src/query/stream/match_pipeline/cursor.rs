use super::*;
use crate::query::plan::join::StageAccess;
use hash::{BuildStorage, HashProbe};
use std::collections::BTreeMap;

#[derive(Debug)]
pub(super) struct HashFrame {
    probe: HashProbe,
    _memory: MemoryReservation,
}

#[derive(Debug)]
pub(super) enum MatchFrame {
    Part(Box<PartCursor>),
    Hash(Box<HashFrame>),
    Consumed,
}

impl MatchFrame {
    pub(super) fn part(mut frame: PartCursor) -> QueryResult<Self> {
        frame
            ._memory
            .grow(std::mem::size_of::<PartCursor>() as u64)?;
        Ok(Self::Part(Box::new(frame)))
    }

    pub(super) fn profile(
        &self,
        step: &MatchStep,
        depth: usize,
        metrics: &QueryMetrics,
    ) -> Option<MatchProfileSlots> {
        match self {
            Self::Part(frame) => frame.profile,
            Self::Hash(_) => metrics
                .match_profile_slots(step.clause_start, step.join.as_ref()?.stages[depth].part),
            Self::Consumed => None,
        }
    }

    fn next_row(
        &mut self,
        step: &MatchStep,
        depth: usize,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        match self {
            Self::Consumed => Ok(None),
            Self::Hash(frame) => {
                let state = bindings
                    .hashes
                    .get_mut(depth)
                    .and_then(Option::as_mut)
                    .ok_or_else(|| QueryError::internal("Hash frame lost its clause owner"))?;
                frame
                    .probe
                    .next_row(state, step, depth, scope, metrics, control)
            }
            Self::Part(frame) => {
                let previous = frame.activate_frame(metrics);
                let result = frame.next_row(scope.0, scope.1, metrics, bindings, control);
                frame.record_fallback_relationship_rows(metrics, &result, true);
                metrics.restore_access_operator(previous);
                result
            }
        }
    }

    fn split_output_memory(
        &mut self,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<MemoryReservation> {
        match self {
            Self::Part(frame) => frame.split_output_memory(row, control),
            Self::Hash(frame) => frame.probe.split_output_memory(row, control),
            Self::Consumed => Err(QueryError::internal("consumed prefix cannot own a row")),
        }
    }
}

impl StepCursor {
    pub(super) fn advance_to(
        &mut self,
        stop: usize,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        loop {
            control.work(1)?;
            let depth = self.stack.len();
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            let next = frame.next_row(&self.step, depth - 1, scope, metrics, bindings, control)?;
            let Some(row) = next else {
                self.stack.pop();
                continue;
            };
            if !self.accept_prefix(depth - 1, &row, scope, params, metrics, control)? {
                continue;
            }
            if depth == stop {
                return Ok(Some(row));
            }
            self.push_frame(row, scope, params, metrics, bindings, control)?;
        }
    }

    pub(super) fn push_frame(
        &mut self,
        row: BindingRow,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let depth = self.stack.len();
        let access = self
            .step
            .join
            .as_ref()
            .map(|plan| plan.stages[depth].access);
        if matches!(access, Some(StageAccess::Hash { .. })) {
            let memory = self
                .stack
                .last_mut()
                .ok_or_else(|| QueryError::internal("Hash cannot be the first stage"))?
                .split_output_memory(&row, control)?;
            return self.push_hash((row, memory), scope, params, metrics, bindings, control);
        }
        let frame = self.frame(depth, &row, scope, metrics, bindings, control)?;
        self.stack.push(MatchFrame::part(frame)?);
        Ok(())
    }

    fn hash_plan(&self) -> QueryResult<&crate::query::plan::join::JoinPlan> {
        self.step
            .join
            .as_deref()
            .ok_or_else(|| QueryError::internal("Hash lacks a plan"))
    }

    fn push_hash(
        &mut self,
        input: (BindingRow, MemoryReservation),
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let depth = self.stack.len();
        let plan = self.hash_plan()?;
        bindings.prepare(plan, scope.0, control)?;
        let slot = hash::hash_slot(metrics, plan, depth);
        let frame_memory = control.reserve(std::mem::size_of::<HashFrame>() as u64)?;
        let mut state = bindings.hashes[depth]
            .take()
            .ok_or_else(|| QueryError::internal("Hash owner was borrowed recursively"))?;
        let key = state.probe_key(&self.step, depth, &input.0, scope, metrics, control)?;
        let probe = if let Some(key) = key {
            state.ensure_build(&self.step, depth, scope.0, scope.1, metrics, control)?;
            self.install_dynamic_filter(depth, &mut state, scope, metrics, control)?;
            match &state.storage {
                BuildStorage::Memory(_) => HashProbe::memory(input, key, &state, control)?,
                BuildStorage::Spill(_) => {
                    state.spill_probe(input, key, control)?;
                    self.partition_prefix(scope, params, metrics, bindings, &mut state, control)?;
                    HashProbe::grace(control)?
                }
                BuildStorage::Empty => return Err(QueryError::internal("Hash build is absent")),
            }
        } else {
            HashProbe::empty()
        };
        self.finish_hash_frame(frame_memory, state, probe, (depth, slot), metrics, bindings)
    }

    fn finish_hash_frame(
        &mut self,
        frame_memory: MemoryReservation,
        mut state: hash::HashBuildState,
        probe: HashProbe,
        position: (usize, Option<usize>),
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
    ) -> QueryResult<()> {
        state.probe_metadata = state.probe_metadata.max(probe.metadata_bytes());
        state.publish(metrics, position.1)?;
        bindings.hashes[position.0] = Some(state);
        self.stack.push(MatchFrame::Hash(Box::new(HashFrame {
            probe,
            _memory: frame_memory,
        })));
        Ok(())
    }

    fn partition_prefix(
        &mut self,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        bindings: &mut MatchReadBindings,
        state: &mut hash::HashBuildState,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let depth = self.stack.len();
        while let Some(row) = self.advance_to(depth, scope, params, metrics, bindings, control)? {
            let memory = self
                .stack
                .last_mut()
                .ok_or_else(|| QueryError::internal("prefix row lost its producer"))?
                .split_output_memory(&row, control)?;
            let input = (row, memory);
            if let Some(key) =
                state.probe_key(&self.step, depth, &input.0, scope, metrics, control)?
            {
                state.spill_probe(input, key, control)?;
            }
        }
        self.stack.resize_with(depth, || MatchFrame::Consumed);
        if let BuildStorage::Spill(store) = &mut state.storage {
            store.temp_logical_bytes(control)?;
        }
        Ok(())
    }
}
