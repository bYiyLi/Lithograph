//! Complete one independently proved arm before reading its filtered partner.

use std::collections::BTreeMap;
use std::rc::Rc;
use std::sync::Arc;
use std::time::Instant;

use super::asp::{arm_step, remove_prefix, shared_node};
use super::table::FactorStorageMetrics;
use super::{FactorTableBuilder, GroupPacket};
use crate::cypher::Value;
use crate::query::expression::{self, BindingRow, BindingValue};
use crate::query::graph::ResolvedGraphView;
use crate::query::plan::{FactorArm, FactorBindingKind, FactorGroup, FactorizedPlan, MatchStep};
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};
use crate::query::stream::profile::FactorizedRuntimeMetrics;
use crate::query::stream::{MatchReadBindings, StepCursor};
use crate::query::{ExecutionControl, MemoryReservation, QueryError, QueryMetrics, QueryResult};
use crate::storage::Snapshot;

#[cfg(test)]
mod tests;

#[derive(Debug)]
pub(crate) struct FactorProducer {
    step: MatchStep,
    plan: Arc<FactorizedPlan>,
    base: BindingRow,
    done: bool,
    stats: FactorizedRuntimeMetrics,
    _memory: MemoryReservation,
}

impl FactorProducer {
    pub(crate) fn new(
        step: &MatchStep,
        mut base: BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let plan = step
            .join
            .as_ref()
            .and_then(|plan| plan.factorized.as_ref())
            .ok_or_else(|| QueryError::internal("factorized cursor lacks its plan"))?;
        if !plan.single_shared {
            return Err(QueryError::internal(
                "factorized shared identity is not unique",
            ));
        }
        base.used_relationships.clear();
        let bytes = checked_add(
            std::mem::size_of::<Self>() as u64,
            expression::binding_row_bytes_controlled(&base, Some(control))?,
        )?;
        Ok(Self {
            step: step.clone(),
            plan: Arc::clone(plan),
            base,
            done: false,
            stats: FactorizedRuntimeMetrics::default(),
            _memory: control.reserve(bytes)?,
        })
    }

    pub(crate) fn next_packet(
        &mut self,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<GroupPacket>> {
        if self.done {
            return Ok(None);
        }
        control.check()?;
        self.check_snapshot(scope.0)?;
        self.done = true;
        let started = Instant::now();
        let completed = self.accumulate(scope, params, metrics, control)?;
        self.stats.accumulate_micros = elapsed_micros(started);
        let Some((mut table, node)) = completed else {
            self.publish(metrics);
            return Ok(None);
        };
        self.stats.prefixes = 1;
        let started = Instant::now();
        self.build_filtered(&mut table, node, scope, params, metrics, control)?;
        self.stats.build_micros = elapsed_micros(started);
        self.stats.group_members = self
            .stats
            .accumulate_rows
            .saturating_add(self.stats.build_rows);
        let mut table = table.finish(control)?;
        self.record_storage(table.storage_metrics());
        self.publish(metrics);
        if table.arm_len(1) == 0 {
            return Ok(None);
        }
        if self.plan.asp.is_some() {
            table = super::asp_runtime::apply(&self.step, table, scope, metrics, control)?;
        }
        // The complete group has passed the total shared predicate; ASP also
        // retained its actual identity equality. Count successful multiplicity,
        // without evaluating a shared property for every Cartesian member.
        metrics.record_profile_rows(
            metrics.match_residual_slot(self.plan.clause_start),
            checked_mul(table.arm_len(0), table.arm_len(1))?,
        );
        let mut packet = GroupPacket::new(Rc::new(table), control)?;
        packet.plan = Some(Arc::clone(&self.plan));
        packet.cartesian_proven = true;
        Ok((!packet.is_empty()).then_some(packet))
    }

    fn publish(&self, metrics: &mut QueryMetrics) {
        let slot = metrics.factorized_slot(self.plan.clause_start, "FactorizedAccumulate");
        metrics.set_factorized_profile(slot, self.stats);
        metrics.record_profile_rows(slot, self.stats.accumulate_rows);
    }

    fn check_snapshot(&self, snapshot: &Snapshot<'_>) -> QueryResult<()> {
        if self
            .step
            .join
            .as_ref()
            .is_none_or(|plan| plan.snapshot_identity != snapshot.cache_identity())
        {
            return Err(QueryError::internal(
                "factorized plan belongs to another Snapshot",
            ));
        }
        Ok(())
    }

    fn arm_cursor(
        &self,
        arm: &FactorArm,
        base: &BindingRow,
        qualify: bool,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<StepCursor> {
        let mut step = arm_step(&self.step, arm, false)?;
        if qualify {
            self.qualify_shared(&mut step, control)?;
        }
        let input = clone_row(base, control)?;
        let mut cursor = StepCursor::new(&step, input.0, control)?;
        drop(input.1);
        let mut ordinals = [0; 2];
        for (slot, hop) in ordinals.iter_mut().zip(&arm.hops) {
            *slot = hop.part;
        }
        cursor.set_profile_parts(&ordinals[..arm.hops.len()], control)?;
        Ok(cursor)
    }

    fn qualify_shared(
        &self,
        step: &mut MatchStep,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let binding = self
            .plan
            .bindings
            .iter()
            .find(|binding| {
                binding.group == FactorGroup::Shared && binding.name == self.plan.shared
            })
            .ok_or_else(|| QueryError::internal("factorized shared proof is missing"))?;
        let FactorBindingKind::Node { labels } = &binding.kind else {
            return Err(QueryError::internal(
                "factorized shared proof is not a Node",
            ));
        };
        for part in &mut step.parts {
            for node in std::iter::once(&mut part.start).chain(part.end.iter_mut()) {
                if node.variable.as_deref() != Some(self.plan.shared.as_str()) {
                    continue;
                }
                for label in labels {
                    control.work(1)?;
                    if !node.labels.contains(label) {
                        node.labels.push(*label);
                    }
                }
            }
        }
        Ok(())
    }

    fn accumulate(
        &mut self,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(FactorTableBuilder, i64)>> {
        let mut cursor = self.arm_cursor(&self.plan.accumulate, &self.base, true, control)?;
        let mut reads = MatchReadBindings::default();
        let mut completed: Option<(FactorTableBuilder, i64)> = None;
        while let Some(row) =
            cursor.next_row(scope.0, scope.1, params, metrics, &mut reads, control)?
        {
            let memory = cursor.take_output_memory(&row, control)?;
            let input = (row, memory);
            if !self.shared_predicate(&input.0, scope, params, metrics, control)? {
                continue;
            }
            self.append_accumulated(&mut completed, input, control)?;
            self.stats.accumulate_rows = self.stats.accumulate_rows.saturating_add(1);
        }
        Ok(completed)
    }

    fn append_accumulated(
        &mut self,
        completed: &mut Option<(FactorTableBuilder, i64)>,
        mut input: (BindingRow, MemoryReservation),
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let node = shared_node(&input.0, &self.plan.shared)?;
        if completed.is_none() {
            let prefix = self.prefix(node, control)?;
            *completed = Some((FactorTableBuilder::new_reserved(prefix, 2, control)?, node));
        }
        let (table, expected) = completed
            .as_mut()
            .ok_or_else(|| QueryError::internal("factorized accumulation lost its prefix"))?;
        if node != *expected {
            return Err(QueryError::internal(
                "UNIQUE factorized prefix changed identity",
            ));
        }
        self.delta(&mut input.0, control)?;
        input.1.resize(expression::binding_row_bytes_controlled(
            &input.0,
            Some(control),
        )?)?;
        table.push_reserved(0, input, control)?;
        self.observe_table(table);
        Ok(())
    }

    fn build_filtered(
        &mut self,
        table: &mut FactorTableBuilder,
        node: i64,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let prefix = self.prefix(node, control)?;
        let mut cursor = self.arm_cursor(&self.plan.build, &prefix.0, false, control)?;
        drop(prefix);
        let mut reads = MatchReadBindings::default();
        while let Some(row) =
            cursor.next_row(scope.0, scope.1, params, metrics, &mut reads, control)?
        {
            let memory = cursor.take_output_memory(&row, control)?;
            let mut input = (row, memory);
            if shared_node(&input.0, &self.plan.shared)? != node {
                return Err(QueryError::internal(
                    "ASP build escaped its completed semijoin",
                ));
            }
            self.delta(&mut input.0, control)?;
            input.1.resize(expression::binding_row_bytes_controlled(
                &input.0,
                Some(control),
            )?)?;
            table.push_reserved(1, input, control)?;
            self.observe_table(table);
            self.stats.build_rows = self.stats.build_rows.saturating_add(1);
        }
        Ok(())
    }

    fn observe_table(&mut self, table: &FactorTableBuilder) {
        self.record_storage(table.storage_metrics());
    }

    fn record_storage(&mut self, storage: FactorStorageMetrics) {
        self.stats.memory_peak_bytes = self.stats.memory_peak_bytes.max(storage.memory_peak_bytes);
        self.stats.spill_encoded_bytes = storage.spill_encoded_bytes;
        self.stats.spill_batches = storage.spill_batches;
        self.stats.spill_vm_steps = storage.spill_vm_steps;
        self.stats.temp_allocated_bytes = storage.temp_allocated_bytes;
    }

    fn shared_predicate(
        &self,
        row: &BindingRow,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        params: &BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let slot = metrics.factorized_slot(self.plan.clause_start, "FactorizedAccumulate");
        metrics.record_factorized_evaluations(slot, 1, 0);
        let previous = metrics.activate_profile_slot(slot);
        let result = expression::evaluate_with_access_control(
            &self.plan.shared_predicate,
            scope.0,
            row,
            params,
            &scope.1.query_access,
            control,
        )
        .and_then(|evaluated| expression::predicate(evaluated.value));
        metrics.restore_access_operator(previous);
        result
    }

    fn prefix(
        &self,
        node: i64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(BindingRow, MemoryReservation)> {
        let mut input = clone_row(&self.base, control)?;
        let order = input
            .0
            .order
            .len()
            .checked_add(1)
            .and_then(usize::checked_next_power_of_two)
            .ok_or_else(|| crate::query::resources::memory_error("factorized order overflow"))?;
        let bytes = checked_add(
            map_entry_bytes::<BindingValue>()?,
            checked_add(
                checked_mul(self.plan.shared.len() as u64, 2)?,
                allocation_bytes::<String>(order.max(4))?,
            )?,
        )?;
        input.1.grow(bytes)?;
        input
            .0
            .insert(self.plan.shared.clone(), BindingValue::Node(node));
        Ok(input)
    }

    fn delta(&self, row: &mut BindingRow, control: &ExecutionControl<'_>) -> QueryResult<()> {
        remove_prefix(row, &self.base, control)?;
        row.values.remove(&self.plan.shared);
        row.order.retain(|name| name != &self.plan.shared);
        Ok(())
    }
}

fn clone_row(
    row: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let memory = control.reserve(expression::binding_row_bytes_controlled(
        row,
        Some(control),
    )?)?;
    Ok((row.clone(), memory))
}

fn elapsed_micros(started: Instant) -> u64 {
    u64::try_from(started.elapsed().as_micros()).unwrap_or(u64::MAX)
}
