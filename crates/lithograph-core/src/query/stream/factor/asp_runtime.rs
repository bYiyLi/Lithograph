//! ASP keeps the selected arm compressed while its identity build is filtered
//! by that arm's complete, deduplicated Node IDs. The other arm stays independent.

use std::time::Instant;

use super::asp::shared_node;
use super::asp_ids::AspNodeIds;
use super::table::FactorStorageMetrics;
use super::{FactorTable, FactorTableBuilder};
use crate::query::expression::{self, BindingRow};
use crate::query::graph::ResolvedGraphView;
use crate::query::plan::{FactorAsp, FactorGroup, MatchStep};
use crate::query::stream::match_pipeline::{HashBuildState, HashProbe};
use crate::query::stream::profile::FactorizedRuntimeMetrics;
use crate::query::{ExecutionControl, MemoryReservation, QueryError, QueryMetrics, QueryResult};
use crate::storage::Snapshot;

pub(super) fn apply(
    step: &MatchStep,
    table: FactorTable,
    scope: (&Snapshot<'_>, &ResolvedGraphView),
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<FactorTable> {
    let plan = step
        .join
        .as_ref()
        .ok_or_else(|| QueryError::internal("ASP lacks its Join plan"))?;
    let asp = plan
        .factorized
        .as_ref()
        .and_then(|plan| plan.asp.as_ref())
        .ok_or_else(|| QueryError::internal("ASP lacks its filtered identity build"))?;
    let depth = plan
        .stages
        .iter()
        .position(|stage| stage.part == asp.part)
        .ok_or_else(|| QueryError::internal("ASP build stage is missing"))?;
    let mut runtime = AspRun::new(step, asp, depth, &table, control)?;
    runtime.run(&table, scope, metrics, control)
}

struct AspRun<'a> {
    step: &'a MatchStep,
    asp: &'a FactorAsp,
    depth: usize,
    probe_arm: usize,
    independent_arm: usize,
    output: Option<FactorTableBuilder>,
    state: HashBuildState,
    stats: FactorizedRuntimeMetrics,
    output_rows: u64,
    _memory: MemoryReservation,
}

impl<'a> AspRun<'a> {
    fn new(
        step: &'a MatchStep,
        asp: &'a FactorAsp,
        depth: usize,
        table: &FactorTable,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let (probe_arm, independent_arm) = match asp.probe_group {
            FactorGroup::Accumulate => (0, 1),
            FactorGroup::Build => (1, 0),
            FactorGroup::Shared => {
                return Err(QueryError::internal(
                    "ASP probe must belong to an independent factor arm",
                ));
            }
        };
        let memory = control.reserve(std::mem::size_of::<Self>() as u64)?;
        let prefix = clone_prefix(table.prefix(), control)?;
        Ok(Self {
            step,
            asp,
            depth,
            probe_arm,
            independent_arm,
            output: Some(FactorTableBuilder::new_reserved(prefix, 2, control)?),
            state: HashBuildState::new(),
            stats: FactorizedRuntimeMetrics::default(),
            output_rows: 0,
            _memory: memory,
        })
    }

    fn run(
        &mut self,
        table: &FactorTable,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<FactorTable> {
        let before = table.storage_metrics();
        let mut keys = self.complete_keys(table, control)?;
        self.stats.semijoin_keys = keys.len();
        let started = Instant::now();
        self.state.ensure_build_from_id_pages(
            self.step,
            self.depth,
            &mut || keys.next_page(control),
            scope,
            metrics,
            control,
        )?;
        self.stats.semijoin_seeks = keys.len();
        self.stats.accumulate_rows = table.arm_len(self.probe_arm);
        self.stats.spill_vm_steps = keys.sqlite_vm_steps();
        self.stats.memory_peak_bytes = keys.memory_peak_bytes();
        self.stats.temp_allocated_bytes = keys.temp_allocated_bytes();
        self.stats.build_micros = elapsed(started);
        drop(keys);
        let started = Instant::now();
        if self.state.is_spilled() {
            self.probe_spilled(table, scope, metrics, control)?;
        } else {
            self.probe_memory(table, scope, metrics, control)?;
        }
        self.stats.probe_micros = elapsed(started);
        let slot = metrics.factorized_slot(self.step.clause_start, "ASPJoin");
        self.state.finish(metrics, None)?;
        self.copy_independent(table, control)?;
        let output = self
            .output
            .take()
            .ok_or_else(missing_output)?
            .finish(control)?;
        self.source_reads(before, table.storage_metrics());
        self.publish(metrics, slot, output.storage_metrics());
        Ok(output)
    }

    fn complete_keys(
        &self,
        table: &FactorTable,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<AspNodeIds> {
        let mut keys = AspNodeIds::new(control)?;
        for ordinal in 0..table.arm_len(self.probe_arm) {
            control.work(1)?;
            let input = member(table, self.probe_arm, ordinal, control)?;
            keys.insert(shared_node(&input.0, &self.asp.probe_variable)?, control)?;
        }
        Ok(keys)
    }

    fn probe_memory(
        &mut self,
        table: &FactorTable,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for ordinal in 0..table.arm_len(self.probe_arm) {
            control.work(1)?;
            let input = member(table, self.probe_arm, ordinal, control)?;
            let mut probe = self
                .state
                .memory_probe(input, self.step, self.depth, scope, metrics, control)?;
            self.drain_probe(&mut probe, scope, metrics, control)?;
        }
        Ok(())
    }

    fn probe_spilled(
        &mut self,
        table: &FactorTable,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for ordinal in 0..table.arm_len(self.probe_arm) {
            control.work(1)?;
            self.state.append_spill_probe(
                member(table, self.probe_arm, ordinal, control)?,
                self.step,
                self.depth,
                scope,
                metrics,
                control,
            )?;
        }
        let mut probe = HashProbe::grace(control)?;
        self.state.record_probe_metadata(&probe);
        self.drain_probe(&mut probe, scope, metrics, control)
    }

    fn drain_probe(
        &mut self,
        probe: &mut HashProbe,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        while let Some(row) = probe.next_row(
            &mut self.state,
            self.step,
            self.depth,
            scope,
            metrics,
            control,
        )? {
            let memory = probe.split_output_memory(&row, control)?;
            let input = (row, memory);
            // Keep the actual identity equality as a residual guard, including
            // the expected build binding, before exposing a correlated member.
            if shared_node(&input.0, &self.asp.probe_variable)?
                != shared_node(&input.0, &self.asp.build_variable)?
            {
                return Err(QueryError::internal(
                    "ASP identity probe returned a different Node",
                ));
            }
            self.output
                .as_mut()
                .ok_or_else(missing_output)?
                .push_reserved(self.probe_arm, input, control)?;
            self.stats.group_members = self.stats.group_members.saturating_add(1);
            self.output_rows = self.output_rows.saturating_add(1);
        }
        Ok(())
    }

    fn copy_independent(
        &mut self,
        table: &FactorTable,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for ordinal in 0..table.arm_len(self.independent_arm) {
            control.work(1)?;
            self.output
                .as_mut()
                .ok_or_else(missing_output)?
                .push_reserved(
                    self.independent_arm,
                    member(table, self.independent_arm, ordinal, control)?,
                    control,
                )?;
            self.stats.group_members = self.stats.group_members.saturating_add(1);
        }
        Ok(())
    }

    fn source_reads(&mut self, before: FactorStorageMetrics, after: FactorStorageMetrics) {
        self.stats.spill_batches = self
            .stats
            .spill_batches
            .saturating_add(after.spill_batches.saturating_sub(before.spill_batches));
        self.stats.spill_vm_steps = self
            .stats
            .spill_vm_steps
            .saturating_add(after.spill_vm_steps.saturating_sub(before.spill_vm_steps));
    }

    fn publish(
        &mut self,
        metrics: &mut QueryMetrics,
        slot: Option<usize>,
        output: FactorStorageMetrics,
    ) {
        let hash = self.state.runtime_stats();
        self.stats.prefixes = 1;
        self.stats.build_rows = hash.build_rows;
        // The complete deduplicated Node ID producer emits each matching Node
        // once, and the proven key is that same non-NULL Node identity.
        self.stats.hash_entries = hash.build_rows;
        self.stats.probe_rows = hash.probe_rows;
        // These are separate actual component peaks. Their maximum is not a
        // simultaneous all-ASP working-set peak; the query execution peak is
        // reported by its existing shared resources ledger.
        self.stats.memory_peak_bytes = self
            .stats
            .memory_peak_bytes
            .max(hash.memory_peak_bytes)
            .max(output.memory_peak_bytes);
        self.stats.spill_encoded_bytes = hash
            .spill_encoded_bytes
            .saturating_add(output.spill_encoded_bytes);
        self.stats.spill_batches = self
            .stats
            .spill_batches
            .saturating_add(hash.spill_batches)
            .saturating_add(output.spill_batches);
        self.stats.spill_vm_steps = self
            .stats
            .spill_vm_steps
            .saturating_add(hash.spill_vm_steps)
            .saturating_add(output.spill_vm_steps);
        self.stats.temp_allocated_bytes = self
            .stats
            .temp_allocated_bytes
            .max(hash.temp_allocated_bytes)
            .max(output.temp_allocated_bytes);
        metrics.set_factorized_profile(slot, self.stats);
        metrics.record_profile_rows(slot, self.output_rows);
    }
}

fn member(
    table: &FactorTable,
    arm: usize,
    ordinal: u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    table
        .read_arm_row(arm, ordinal, control)?
        .ok_or_else(|| QueryError::internal("ASP factor member ended before its recorded length"))
}

fn clone_prefix(
    prefix: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let memory = control.reserve(expression::binding_row_bytes_controlled(
        prefix,
        Some(control),
    )?)?;
    Ok((prefix.clone(), memory))
}

fn missing_output() -> QueryError {
    QueryError::internal("ASP output table is closed")
}

fn elapsed(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}
