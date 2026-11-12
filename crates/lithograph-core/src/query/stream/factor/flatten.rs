//! One suspended factor product. Only the selected row of each zipped arm is
//! resident; the last arm advances fastest and no Cartesian row vector exists.

use super::table::FactorStorageMetrics;
use super::{BindingPacket, GroupPacket, GroupProjection};
use crate::query::expression::{self, BindingRow, BindingValue};
use crate::query::resources::{allocation_bytes, checked_add};
use crate::query::stream::FactorizedRuntimeMetrics;
use crate::query::{ExecutionControl, MemoryReservation, QueryError, QueryResult};

#[derive(Debug)]
struct ArmRow {
    row: BindingRow,
    _memory: MemoryReservation,
}

#[derive(Debug)]
struct GroupRun {
    packet: GroupPacket,
    positions: Vec<u64>,
    rows: Vec<Option<ArmRow>>,
    scratch: BindingRow,
    scratch_memory: MemoryReservation,
    _memory: MemoryReservation,
    started: bool,
    empty: bool,
    io_last: FactorStorageMetrics,
    memory_peak_bytes: u64,
}

#[derive(Debug)]
enum Pending {
    Flat(ArmRow),
    Group(GroupRun),
}

#[derive(Debug)]
pub(crate) struct FlattenContinuation {
    pending: Option<Pending>,
    output_memory: Option<MemoryReservation>,
    _memory: MemoryReservation,
}

impl FlattenContinuation {
    pub(crate) fn take_storage_metrics(
        &mut self,
    ) -> QueryResult<Option<(usize, FactorizedRuntimeMetrics)>> {
        let Some(Pending::Group(run)) = self.pending.as_mut() else {
            return Ok(None);
        };
        let Some(clause) = run.packet.plan.as_ref().map(|plan| plan.clause_start) else {
            return Ok(None);
        };
        let current = run.packet.table.storage_metrics();
        let delta = FactorizedRuntimeMetrics {
            memory_peak_bytes: checked_add(run.memory_peak_bytes, self._memory.bytes())?,
            spill_encoded_bytes: current
                .spill_encoded_bytes
                .saturating_sub(run.io_last.spill_encoded_bytes),
            spill_batches: current
                .spill_batches
                .saturating_sub(run.io_last.spill_batches),
            spill_vm_steps: current
                .spill_vm_steps
                .saturating_sub(run.io_last.spill_vm_steps),
            temp_allocated_bytes: current.temp_allocated_bytes,
            ..FactorizedRuntimeMetrics::default()
        };
        run.io_last = current;
        Ok(Some((clause, delta)))
    }

    pub(crate) fn clause_start(&self) -> Option<usize> {
        match self.pending.as_ref()? {
            Pending::Group(run) => run.packet.plan.as_ref().map(|plan| plan.clause_start),
            Pending::Flat(_) => None,
        }
    }

    pub(crate) fn new(control: &ExecutionControl<'_>) -> QueryResult<Self> {
        Ok(Self {
            pending: None,
            output_memory: None,
            _memory: control.reserve(std::mem::size_of::<Self>() as u64)?,
        })
    }

    pub(crate) fn install(
        &mut self,
        packet: BindingPacket,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        self.require_empty()?;
        self.output_memory = None;
        self.pending = Some(match packet {
            BindingPacket::Flat(row) => {
                let memory = control.reserve(expression::binding_row_bytes_controlled(
                    &row,
                    Some(control),
                )?)?;
                Pending::Flat(ArmRow {
                    row,
                    _memory: memory,
                })
            }
            BindingPacket::Group(packet) => Pending::Group(GroupRun::new(packet, control)?),
        });
        Ok(())
    }

    fn require_empty(&self) -> QueryResult<()> {
        if self.pending.is_some() {
            return Err(QueryError::internal(
                "factor product replaced before exhaustion",
            ));
        }
        Ok(())
    }

    /// The callback may project directly into the existing Value boundary.
    /// The prefix is copied once per group, never once per final result row.
    pub(crate) fn with_next_row<T>(
        &mut self,
        control: &ExecutionControl<'_>,
        visit: impl FnOnce(&BindingRow, Option<&[GroupProjection]>) -> QueryResult<T>,
    ) -> QueryResult<Option<T>> {
        if let Err(error) = control.check() {
            self.pending = None;
            self.output_memory = None;
            return Err(error);
        }
        self.output_memory = None;
        let result = match self.pending.as_mut() {
            Some(Pending::Flat(_)) => {
                let Some(Pending::Flat(row)) = self.pending.take() else {
                    return Err(QueryError::internal(
                        "factor Flat row changed while borrowed",
                    ));
                };
                visit(&row.row, None).map(Some)
            }
            Some(Pending::Group(run)) => match run.prepare_row(control) {
                Ok(true) => visit(&run.scratch, run.packet.projection.as_deref()).map(Some),
                Ok(false) => Ok(None),
                Err(error) => Err(error),
            },
            None => Ok(None),
        };
        if !matches!(result, Ok(Some(_))) {
            self.pending = None;
        }
        result
    }

    /// Owned-row consumers pay for their final copy and transfer its lease via
    /// split_output_memory, just like the existing MATCH row cursor.
    pub(crate) fn next_row(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        let mut memory = None;
        let row = self.with_next_row(control, |row, _| {
            let claim = control.reserve(expression::binding_row_bytes_controlled(
                row,
                Some(control),
            )?)?;
            let copied = expression::clone_binding_row_unreserved_controlled(row, Some(control))?;
            memory = Some(claim);
            Ok(copied)
        })?;
        self.output_memory = memory;
        Ok(row)
    }

    pub(crate) fn split_output_memory(
        &mut self,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<MemoryReservation> {
        let bytes = expression::binding_row_bytes_controlled(row, Some(control))?;
        self.output_memory
            .as_mut()
            .ok_or_else(|| QueryError::internal("factor output has no owned-row lease"))?
            .split(bytes)
    }
}

impl GroupRun {
    fn new(packet: GroupPacket, control: &ExecutionControl<'_>) -> QueryResult<Self> {
        let arms = packet.table.arm_count();
        let io_last = packet.table.storage_metrics();
        if group_is_empty(&packet, control)? {
            return Ok(Self {
                packet,
                positions: Vec::new(),
                rows: Vec::new(),
                scratch: BindingRow::default(),
                scratch_memory: control.reserve(0)?,
                _memory: control.reserve(0)?,
                started: false,
                empty: true,
                io_last,
                memory_peak_bytes: 0,
            });
        }
        let memory = control.reserve(checked_add(
            allocation_bytes::<u64>(arms)?,
            allocation_bytes::<Option<ArmRow>>(arms)?,
        )?)?;
        let prefix = packet.table.prefix();
        let scratch_memory = control.reserve(expression::binding_row_bytes_controlled(
            prefix,
            Some(control),
        )?)?;
        let scratch = expression::clone_binding_row_unreserved_controlled(prefix, Some(control))?;
        let mut rows = Vec::with_capacity(arms);
        rows.resize_with(arms, || None);
        Ok(Self {
            packet,
            positions: vec![0; arms],
            rows,
            scratch,
            scratch_memory,
            _memory: memory,
            started: false,
            empty: false,
            io_last,
            memory_peak_bytes: 0,
        })
    }

    fn prepare_row(&mut self, control: &ExecutionControl<'_>) -> QueryResult<bool> {
        while !self.empty {
            control.work(1)?;
            self.reset_scratch(control)?;
            let changed = if self.started {
                let Some(arm) = self.advance() else {
                    self.empty = true;
                    return Ok(false);
                };
                arm
            } else {
                self.started = true;
                0
            };
            self.load_rows(changed, control)?;
            self.reserve_merge(control)?;
            self.observe_memory(0)?;
            if self.merge_rows(control)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn advance(&mut self) -> Option<usize> {
        for arm in (0..self.positions.len()).rev() {
            let next = self.positions[arm].checked_add(1)?;
            if next < self.packet.table.arm_len(arm) {
                self.positions[arm] = next;
                self.positions[arm + 1..].fill(0);
                return Some(arm);
            }
        }
        None
    }

    fn load_rows(&mut self, first: usize, control: &ExecutionControl<'_>) -> QueryResult<()> {
        for arm in first..self.rows.len() {
            control.work(1)?;
            self.rows[arm] = None;
            let (row, memory, peak) = self
                .packet
                .table
                .read_arm_row_measured(arm, self.positions[arm], control)?
                .ok_or_else(|| {
                    QueryError::internal("factor arm ended before its recorded length")
                })?;
            self.observe_memory(peak)?;
            self.rows[arm] = Some(ArmRow {
                row,
                _memory: memory,
            });
        }
        Ok(())
    }

    fn observe_memory(&mut self, transient: u64) -> QueryResult<()> {
        let mut bytes = checked_add(
            self.packet.table.resident_bytes()?,
            self.packet.memory_bytes()?,
        )?;
        bytes = checked_add(
            bytes,
            checked_add(self._memory.bytes(), self.scratch_memory.bytes())?,
        )?;
        for row in self.rows.iter().flatten() {
            bytes = checked_add(bytes, row._memory.bytes())?;
        }
        self.memory_peak_bytes = self.memory_peak_bytes.max(checked_add(bytes, transient)?);
        Ok(())
    }

    fn reset_scratch(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let prefix = self.packet.table.prefix();
        control.work(self.scratch.values.len() as u64)?;
        self.scratch
            .values
            .retain(|name, _| prefix.values.contains_key(name));
        self.scratch.order.truncate(prefix.order.len());
        self.scratch
            .used_relationships
            .retain(|id| prefix.used_relationships.contains(id));
        let bytes = expression::binding_row_bytes_controlled(&self.scratch, Some(control))?;
        self.scratch_memory.resize(bytes)
    }

    fn reserve_merge(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let mut bytes = self.scratch_memory.bytes();
        let mut names = self.scratch.order.len();
        for row in self.rows.iter().flatten() {
            control.work(1)?;
            bytes = checked_add(
                bytes,
                expression::binding_row_bytes_controlled(&row.row, Some(control))?,
            )?;
            names = names.checked_add(row.row.order.len()).ok_or_else(|| {
                crate::query::resources::memory_error("factor output column count overflow")
            })?;
        }
        let capacity = names.max(4).checked_next_power_of_two().ok_or_else(|| {
            crate::query::resources::memory_error("factor output column capacity overflow")
        })?;
        self.scratch_memory
            .resize(checked_add(bytes, allocation_bytes::<String>(capacity)?)?)
    }

    fn merge_rows(&mut self, control: &ExecutionControl<'_>) -> QueryResult<bool> {
        for row in self.rows.iter().flatten() {
            for id in &row.row.used_relationships {
                control.work(1)?;
                if !self.scratch.used_relationships.insert(*id) {
                    return Ok(false);
                }
            }
            for name in &row.row.order {
                control.work(1)?;
                let value = row.row.values.get(name).ok_or_else(|| {
                    QueryError::internal("factor arm order references a missing binding")
                })?;
                if let Some(existing) = self.scratch.values.get(name) {
                    if !same_binding(existing, value, control)? {
                        return Ok(false);
                    }
                    continue;
                }
                self.scratch.insert(
                    expression::clone_string_controlled(name, Some(control))?,
                    expression::clone_binding_value_unreserved_controlled(value, Some(control))?,
                );
            }
        }
        Ok(true)
    }
}

fn group_is_empty(packet: &GroupPacket, control: &ExecutionControl<'_>) -> QueryResult<bool> {
    if packet.table.arm_count() == 0 {
        return Ok(true);
    }
    for arm in 0..packet.table.arm_count() {
        control.work(1)?;
        if packet.table.arm_len(arm) == 0 {
            return Ok(true);
        }
    }
    Ok(false)
}

fn same_binding(
    left: &BindingValue,
    right: &BindingValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    control.work(1)?;
    match (left, right) {
        (BindingValue::Node(left), BindingValue::Node(right)) => Ok(left == right),
        (BindingValue::Relationship(left), BindingValue::Relationship(right)) => {
            Ok(left.id == right.id)
        }
        (BindingValue::Null, BindingValue::Null) => Ok(true),
        (BindingValue::Scalar(left), BindingValue::Scalar(right)) => {
            expression::cypher_equals_controlled(left, right, Some(control))
                .map(|equal| equal == Some(true))
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests;
