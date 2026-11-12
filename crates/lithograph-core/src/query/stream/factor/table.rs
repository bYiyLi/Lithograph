//! One query-owned prefix and typed independent arm sequences. TEMP contains
//! only those sequences, never their Cartesian product.

use std::cell::{Cell, RefCell};
use std::mem::size_of;

use rusqlite::Connection;

use super::compact::{CompactLayout, CompactMember, PackResult};

use crate::query::expression::{self, BindingRow};
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, memory_error};
use crate::query::spill::{
    BindingSpill, BindingSpillIo, open_spill_connection, private_temp_error, serialization_bytes,
};
use crate::query::{ExecutionControl, MemoryReservation, QueryError, QueryResources, QueryResult};

pub(crate) const FACTOR_RESIDENT_BYTES: u64 = 256 * 1024;
pub(crate) const FACTOR_PRIVATE_RUST_BYTES: u64 = 4096;
const PRIVATE_ARM_NAME_BYTES: u64 = 128;
const MAX_ARMS: usize = 128;

#[derive(Debug, Default, Clone, Copy)]
pub(crate) struct FactorStorageMetrics {
    pub(crate) memory_peak_bytes: u64,
    pub(crate) spill_encoded_bytes: u64,
    pub(crate) spill_batches: u64,
    pub(crate) spill_vm_steps: u64,
    /// Observed logical TEMP pager allocation, not OS-allocated file bytes.
    pub(crate) temp_allocated_bytes: u64,
}

type Member = CompactMember;

struct MemoryPack {
    fallback: Option<(BindingRow, MemoryReservation)>,
    peak_bytes: u64,
}

#[derive(Debug)]
struct Arm {
    layout: Option<CompactLayout>,
    rows: Vec<Member>,
    buffer_memory: MemoryReservation,
    spill: Option<BindingSpill>,
    len: u64,
    row_heap_bytes: u64,
}

#[derive(Debug)]
struct PrivateStore {
    connection: RefCell<Option<Connection>>,
    _memory: MemoryReservation,
    metrics: Cell<FactorStorageMetrics>,
}

#[derive(Debug)]
pub(crate) struct FactorTable {
    prefix: BindingRow,
    prefix_memory: MemoryReservation,
    arms: Vec<Arm>,
    metadata_memory: MemoryReservation,
    private: Option<Box<PrivateStore>>,
    resources: QueryResources,
    max_row_bytes: u64,
    memory_peak_bytes: Cell<u64>,
}

#[derive(Debug)]
pub(crate) struct FactorTableBuilder {
    table: FactorTable,
    failed: bool,
}

pub(crate) fn factor_member_slot_bytes() -> u64 {
    size_of::<Member>() as u64
}

pub(crate) fn factor_table_metadata_bytes(arm_count: usize) -> QueryResult<u64> {
    checked_add(
        (size_of::<FactorTable>() + size_of::<Member>() + 2 * size_of::<usize>()) as u64,
        allocation_bytes::<Arm>(arm_count)?,
    )
}

pub(crate) fn factor_private_metadata_bytes(arm_count: usize) -> QueryResult<u64> {
    checked_add(
        FACTOR_PRIVATE_RUST_BYTES,
        checked_mul(arm_count as u64, PRIVATE_ARM_NAME_BYTES)?,
    )
}

impl FactorTableBuilder {
    pub(crate) fn storage_metrics(&self) -> FactorStorageMetrics {
        self.table.storage_metrics()
    }
    #[cfg(test)]
    pub(crate) fn resident_bytes(&self) -> QueryResult<u64> {
        self.table.resident_bytes()
    }

    #[cfg(test)]
    pub(crate) fn new(
        prefix: BindingRow,
        arm_count: usize,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let memory = control.reserve(expression::binding_row_bytes_controlled(
            &prefix,
            Some(control),
        )?)?;
        Self::new_reserved((prefix, memory), arm_count, control)
    }

    pub(crate) fn new_reserved(
        prefix: (BindingRow, MemoryReservation),
        arm_count: usize,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        control.check()?;
        if arm_count > MAX_ARMS {
            return Err(memory_error(
                "factorized arm count exceeds the bounded MATCH plan",
            ));
        }
        let (prefix, mut prefix_memory) = checked_input(prefix, control)?;
        let mut metadata_memory = control.reserve(factor_table_metadata_bytes(arm_count)?)?;
        let mut arms = Vec::new();
        arms.try_reserve_exact(arm_count)
            .map_err(|_| memory_error("unable to allocate factorized arms"))?;
        metadata_memory.resize(factor_table_metadata_bytes(arms.capacity())?)?;
        for _ in 0..arm_count {
            control.work(1)?;
            arms.push(Arm {
                layout: None,
                rows: Vec::new(),
                buffer_memory: control.reserve(0)?,
                spill: None,
                len: 0,
                row_heap_bytes: 0,
            });
        }
        let initial_peak = checked_add(prefix_memory.bytes(), metadata_memory.bytes())?;
        prefix_memory.resize(
            prefix_memory
                .bytes()
                .saturating_sub(size_of::<BindingRow>() as u64),
        )?;
        Ok(Self {
            table: FactorTable {
                prefix,
                prefix_memory,
                arms,
                metadata_memory,
                private: None,
                resources: control.resources().clone(),
                max_row_bytes: 0,
                memory_peak_bytes: Cell::new(initial_peak),
            },
            failed: false,
        })
    }

    #[cfg(test)]
    pub(crate) fn push(
        &mut self,
        arm: usize,
        delta: BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.failed {
            return Err(closed_builder());
        }
        let input = expression::binding_row_bytes_controlled(&delta, Some(control))
            .and_then(|bytes| control.reserve(bytes))
            .map(|memory| (delta, memory));
        match input {
            Ok(input) => self.push_reserved(arm, input, control),
            Err(error) => self.fail(error),
        }
    }

    pub(crate) fn push_reserved(
        &mut self,
        arm: usize,
        input: (BindingRow, MemoryReservation),
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.failed {
            return Err(closed_builder());
        }
        match self.table.push(arm, input, control) {
            Ok(()) => Ok(()),
            Err(error) => self.fail(error),
        }
    }

    fn fail(&mut self, error: QueryError) -> QueryResult<()> {
        self.failed = true;
        self.table.abort();
        Err(error)
    }

    pub(crate) fn finish(self, control: &ExecutionControl<'_>) -> QueryResult<FactorTable> {
        if self.failed {
            return Err(closed_builder());
        }
        self.table.check_owner(control)?;
        if let Some(private) = &self.table.private {
            private.refresh_allocation(control)?;
        }
        self.table.observe_memory(0)?;
        Ok(self.table)
    }
}

impl FactorTable {
    pub(crate) fn storage_metrics(&self) -> FactorStorageMetrics {
        let mut metrics = self
            .private
            .as_ref()
            .map_or_else(FactorStorageMetrics::default, |store| store.metrics.get());
        metrics.memory_peak_bytes = self.memory_peak_bytes.get();
        metrics
    }

    fn observe_memory(&self, extra: u64) -> QueryResult<()> {
        let bytes = checked_add(self.resident_bytes()?, extra)?;
        self.observe_peak(bytes);
        Ok(())
    }

    fn observe_peak(&self, bytes: u64) {
        self.memory_peak_bytes
            .set(self.memory_peak_bytes.get().max(bytes));
    }
    pub(crate) fn prefix(&self) -> &BindingRow {
        &self.prefix
    }
    pub(crate) fn arm_count(&self) -> usize {
        self.arms.len()
    }
    pub(crate) fn arm_len(&self, arm: usize) -> u64 {
        self.arms.get(arm).map_or(0, |arm| arm.len)
    }

    #[cfg(test)]
    pub(crate) fn is_spilled(&self) -> bool {
        self.private.is_some()
    }

    pub(crate) fn resident_bytes(&self) -> QueryResult<u64> {
        let mut bytes = checked_add(self.prefix_memory.bytes(), self.metadata_memory.bytes())?;
        if let Some(private) = &self.private {
            bytes = checked_add(bytes, private._memory.bytes())?;
        }
        for arm in &self.arms {
            bytes = checked_add(bytes, arm.buffer_memory.bytes())?;
            bytes = checked_add(bytes, arm.row_heap_bytes)?;
            if let Some(layout) = &arm.layout {
                bytes = checked_add(bytes, layout.owned_bytes())?;
            }
        }
        Ok(bytes)
    }

    fn check_owner(&self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        control.check()?;
        let mut owner = self.resources.reserve(0)?;
        owner.merge(control.reserve(0)?)
    }

    fn push(
        &mut self,
        arm: usize,
        input: (BindingRow, MemoryReservation),
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        self.check_owner(control)?;
        let (row, memory) = checked_input(input, control)?;
        self.max_row_bytes = self.max_row_bytes.max(memory.bytes());
        let next = next_arm_len(&self.arms, arm)?;
        if self.private.is_none() && self.needs_spill(arm, memory.bytes())? {
            self.spill(memory.bytes(), control)?;
        }
        self.append(arm, (row, memory), control)?;
        self.arms[arm].len = next;
        self.observe_memory(0)?;
        Ok(())
    }

    fn append(
        &mut self,
        arm: usize,
        input: (BindingRow, MemoryReservation),
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.private.is_some() {
            return self.append_spilled(arm, input, control);
        }
        let retained = self.resident_bytes()?;
        let packed = pack_memory(&mut self.arms[arm], input, retained, control)?;
        self.observe_peak(packed.peak_bytes);
        if let Some(input) = packed.fallback {
            self.spill(input.1.bytes(), control)?;
            self.append_spilled(arm, input, control)?;
        }
        Ok(())
    }

    fn append_spilled(
        &mut self,
        arm: usize,
        input: (BindingRow, MemoryReservation),
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let (row, memory) = input;
        let private = self.private.as_ref().ok_or_else(closed_private)?;
        let io = push_spilled(private, &mut self.arms[arm], &row, &self.resources, control)?;
        self.observe_memory(checked_add(memory.bytes(), io.memory_peak_bytes)?)
    }

    fn needs_spill(&self, arm: usize, incoming: u64) -> QueryResult<bool> {
        let resident = self.resident_bytes()?;
        let outside = self
            .resources
            .used()
            .saturating_sub(resident)
            .saturating_sub(incoming);
        let headroom = checked_add(
            // The next producer input is claimed before push can release any
            // retained member. Leave that real overlap available for flushing.
            checked_add(
                serialization_bytes(self.max_row_bytes)?,
                checked_mul(self.max_row_bytes, 2)?,
            )?,
            factor_private_metadata_bytes(self.arms.len())?,
        )?;
        let target = self
            .resources
            .limit()
            .map_or(FACTOR_RESIDENT_BYTES, |limit| {
                FACTOR_RESIDENT_BYTES.min(limit.saturating_sub(outside).saturating_sub(headroom))
            });
        let arm = &self.arms[arm];
        let growth = if arm.rows.len() == arm.rows.capacity() {
            allocation_bytes::<Member>(next_member_capacity(arm.rows.capacity())?)?
        } else {
            0
        };
        Ok(checked_add(checked_add(resident, incoming)?, growth)? > target)
    }

    fn spill(&mut self, pending: u64, control: &ExecutionControl<'_>) -> QueryResult<()> {
        // This claim outlives every Arm's BindingSpill identifier allocation.
        self.metadata_memory
            .grow(checked_mul(self.arms.len() as u64, PRIVATE_ARM_NAME_BYTES)?)?;
        self.private = Some(Box::new(PrivateStore::new(control)?));
        self.observe_memory(pending)?;
        for position in 0..self.arms.len() {
            control.work(1)?;
            let retained = checked_add(self.resident_bytes()?, pending)?;
            let private = self.private.as_ref().ok_or_else(closed_private)?;
            let peak = spill_arm(
                private,
                &mut self.arms[position],
                &self.resources,
                retained,
                control,
            )?;
            self.observe_peak(peak);
        }
        Ok(())
    }

    pub(crate) fn read_arm_row(
        &self,
        arm: usize,
        ordinal: u64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(BindingRow, MemoryReservation)>> {
        self.read_arm_row_measured(arm, ordinal, control)
            .map(|row| row.map(|(row, memory, _)| (row, memory)))
    }

    pub(crate) fn read_arm_row_measured(
        &self,
        arm: usize,
        ordinal: u64,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(BindingRow, MemoryReservation, u64)>> {
        self.check_owner(control)?;
        let arm = self
            .arms
            .get(arm)
            .ok_or_else(|| QueryError::internal("factorized arm is absent"))?;
        if ordinal >= arm.len {
            return Ok(None);
        }
        if let Some(private) = &self.private {
            let result = read_spilled(private, arm, ordinal, control);
            if result.is_err() {
                private.close();
            }
            return result.and_then(|(row, memory, peak)| {
                self.observe_memory(peak)?;
                Ok(Some((row, memory, peak)))
            });
        }
        let position = usize::try_from(ordinal)
            .map_err(|_| memory_error("factorized member position overflow"))?;
        let member = arm
            .rows
            .get(position)
            .ok_or_else(|| QueryError::internal("factorized member is absent"))?;
        let (row, memory, peak) = arm
            .layout
            .as_ref()
            .ok_or_else(|| QueryError::internal("factorized arm layout is absent"))?
            .read_measured(member, control)?;
        self.observe_memory(peak)?;
        Ok(Some((row, memory, peak)))
    }

    fn abort(&mut self) {
        self.private = None;
        self.arms = Vec::new();
        self.prefix = BindingRow::default();
        let _ = self.prefix_memory.resize(0);
        let _ = self.metadata_memory.resize(0);
    }
}

impl Drop for FactorTable {
    fn drop(&mut self) {
        self.private = None;
    }
}

impl PrivateStore {
    fn new(control: &ExecutionControl<'_>) -> QueryResult<Self> {
        control.check()?;
        let memory = control.reserve(FACTOR_PRIVATE_RUST_BYTES)?;
        let connection = open_spill_connection().map_err(private_temp_error)?;
        let vm_steps = {
            let mut statement = connection
                .prepare("BEGIN")
                .map_err(|error| private_temp_error(error.into()))?;
            statement
                .execute([])
                .map_err(|error| private_temp_error(error.into()))?;
            vm_steps(&statement)?
        };
        Ok(Self {
            connection: RefCell::new(Some(connection)),
            _memory: memory,
            metrics: Cell::new(FactorStorageMetrics {
                spill_vm_steps: vm_steps,
                ..Default::default()
            }),
        })
    }

    fn create_arm(&self, resources: &QueryResources) -> QueryResult<BindingSpill> {
        let (spill, vm) =
            self.apply(|connection| BindingSpill::create_measured(connection, resources.clone()))?;
        let mut metrics = self.metrics.get();
        metrics.spill_vm_steps = metrics.spill_vm_steps.saturating_add(vm);
        self.metrics.set(metrics);
        Ok(spill)
    }

    fn record_io(&self, io: BindingSpillIo, writing: bool) {
        let mut metrics = self.metrics.get();
        if writing {
            metrics.spill_encoded_bytes =
                metrics.spill_encoded_bytes.saturating_add(io.encoded_bytes);
        }
        metrics.spill_vm_steps = metrics.spill_vm_steps.saturating_add(io.sqlite_vm_steps);
        metrics.spill_batches = metrics.spill_batches.saturating_add(1);
        self.metrics.set(metrics);
    }

    fn refresh_allocation(&self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        control.check()?;
        let (pages, page_vm) =
            self.apply(|connection| pager_value(connection, "PRAGMA temp.page_count"))?;
        let (size, size_vm) =
            self.apply(|connection| pager_value(connection, "PRAGMA temp.page_size"))?;
        let allocated = checked_mul(pages, size)?;
        let mut metrics = self.metrics.get();
        metrics.spill_vm_steps = metrics
            .spill_vm_steps
            .saturating_add(page_vm)
            .saturating_add(size_vm);
        metrics.temp_allocated_bytes = metrics.temp_allocated_bytes.max(allocated);
        self.metrics.set(metrics);
        Ok(())
    }

    fn apply<T>(
        &self,
        use_connection: impl FnOnce(&Connection) -> QueryResult<T>,
    ) -> QueryResult<T> {
        let result = {
            let connection = self.connection.borrow();
            let connection = connection.as_ref().ok_or_else(closed_private)?;
            use_connection(connection).map_err(private_temp_error)
        };
        if result.is_err() {
            self.close();
        }
        result
    }

    fn close(&self) {
        if let Some(connection) = self.connection.borrow_mut().take() {
            let _ = connection.execute_batch("ROLLBACK");
            drop(connection);
        }
    }
}

impl Drop for PrivateStore {
    fn drop(&mut self) {
        self.close();
    }
}

fn checked_input(
    input: (BindingRow, MemoryReservation),
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let bytes = expression::binding_row_bytes_controlled(&input.0, Some(control))?;
    let mut memory = control.reserve(0)?;
    memory.merge(input.1)?;
    if memory.bytes() < bytes {
        return Err(QueryError::internal(
            "factorized input is not fully reserved",
        ));
    }
    memory.resize(bytes)?;
    Ok((input.0, memory))
}

fn next_arm_len(arms: &[Arm], position: usize) -> QueryResult<u64> {
    arms.get(position)
        .ok_or_else(|| QueryError::internal("factorized arm is absent"))?
        .len
        .checked_add(1)
        .filter(|len| *len <= i64::MAX as u64)
        .ok_or_else(|| memory_error("factorized arm cardinality overflow"))
}

fn pack_memory(
    arm: &mut Arm,
    input: (BindingRow, MemoryReservation),
    retained: u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<MemoryPack> {
    let old_layout = arm.layout.as_ref().map_or(0, CompactLayout::owned_bytes);
    if arm.layout.is_none() {
        arm.layout = CompactLayout::try_new(&input.0, control)?;
    }
    let layout_added = arm
        .layout
        .as_ref()
        .map_or(0, CompactLayout::owned_bytes)
        .saturating_sub(old_layout);
    let retained = checked_add(retained, layout_added)?;
    let incoming = input.1.bytes();
    let mut peak = checked_add(retained, incoming)?;
    let Some(layout) = &arm.layout else {
        return Ok(MemoryPack {
            fallback: Some(input),
            peak_bytes: peak,
        });
    };
    let member = match layout.pack(input, control)? {
        PackResult::Packed(member) => member,
        PackResult::Fallback(input) => {
            return Ok(MemoryPack {
                fallback: Some(input),
                peak_bytes: peak,
            });
        }
    };
    // Packing first claims only the new cell/id buffers beside the entire
    // producer row. The Scalar/CSV heaps are moved, so do not count them twice.
    peak = peak.max(checked_add(
        checked_add(retained, incoming)?,
        member.buffer_bytes()?,
    )?);
    if arm.rows.len() == arm.rows.capacity() {
        let replacement_bytes = grow_members(arm, control)?;
        // The old member Vec and the pending compact member remain claimed
        // until the replacement Vec is installed.
        peak = peak.max(checked_add(
            checked_add(retained, member.owned_bytes())?,
            replacement_bytes,
        )?);
    }
    arm.row_heap_bytes = checked_add(arm.row_heap_bytes, member.owned_bytes())?;
    arm.rows.push(member);
    Ok(MemoryPack {
        fallback: None,
        peak_bytes: peak,
    })
}

fn grow_members(arm: &mut Arm, control: &ExecutionControl<'_>) -> QueryResult<u64> {
    let capacity = next_member_capacity(arm.rows.capacity())?;
    let mut memory = control.reserve(allocation_bytes::<Member>(capacity)?)?;
    let mut rows = Vec::new();
    rows.try_reserve_exact(capacity)
        .map_err(|_| memory_error("unable to allocate factorized members"))?;
    memory.resize(allocation_bytes::<Member>(rows.capacity())?)?;
    rows.append(&mut arm.rows);
    let previous = std::mem::replace(&mut arm.rows, rows);
    drop(previous);
    arm.buffer_memory = memory;
    Ok(arm.buffer_memory.bytes())
}

fn spill_arm(
    private: &PrivateStore,
    arm: &mut Arm,
    resources: &QueryResources,
    retained: u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let mut peak = retained;
    if !arm.rows.is_empty() {
        let mut spill = private.create_arm(resources)?;
        let layout = arm
            .layout
            .as_ref()
            .ok_or_else(|| QueryError::internal("factorized arm layout is absent"))?;
        for member in &arm.rows {
            control.work(1)?;
            let (row, memory, read_peak) = layout.read_measured(member, control)?;
            let io = private.apply(|connection| spill.push_measured(connection, &row, control))?;
            private.record_io(io, true);
            let encode_peak = checked_add(memory.bytes(), io.memory_peak_bytes)?;
            peak = peak.max(checked_add(retained, read_peak.max(encode_peak))?);
        }
        arm.spill = Some(spill);
    }
    arm.layout = None;
    arm.rows = Vec::new();
    arm.buffer_memory.resize(0)?;
    arm.row_heap_bytes = 0;
    Ok(peak)
}

fn next_member_capacity(capacity: usize) -> QueryResult<usize> {
    capacity
        .max(2)
        .checked_mul(2)
        .ok_or_else(|| memory_error("factorized member capacity overflow"))
}

fn push_spilled(
    private: &PrivateStore,
    arm: &mut Arm,
    row: &BindingRow,
    resources: &QueryResources,
    control: &ExecutionControl<'_>,
) -> QueryResult<BindingSpillIo> {
    if arm.spill.is_none() {
        arm.spill = Some(private.create_arm(resources)?);
    }
    let spill = arm.spill.as_mut().ok_or_else(closed_private)?;
    let io = private.apply(|connection| spill.push_measured(connection, row, control))?;
    private.record_io(io, true);
    if private.metrics.get().spill_batches % 256 == 1 {
        private.refresh_allocation(control)?;
    }
    Ok(io)
}

fn read_spilled(
    private: &PrivateStore,
    arm: &Arm,
    ordinal: u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation, u64)> {
    let spill = arm
        .spill
        .as_ref()
        .ok_or_else(|| QueryError::internal("factorized TEMP arm is absent"))?;
    let sequence =
        i64::try_from(ordinal).map_err(|_| memory_error("factorized member position overflow"))?;
    let (mut rows, mut memory, io) = private.apply(|connection| {
        spill.read_batch_reserved_measured(connection, sequence - 1, 1, control)
    })?;
    private.record_io(io, false);
    let (actual, row) = rows
        .pop()
        .filter(|(actual, _)| *actual == sequence)
        .ok_or_else(|| QueryError::internal("factorized TEMP member is absent"))?;
    let _ = actual;
    drop(rows);
    memory.resize(expression::binding_row_bytes_controlled(
        &row,
        Some(control),
    )?)?;
    Ok((row, memory, io.memory_peak_bytes))
}

fn vm_steps(statement: &rusqlite::Statement<'_>) -> QueryResult<u64> {
    u64::try_from(statement.reset_status(rusqlite::StatementStatus::VmStep))
        .map_err(|_| memory_error("factorized TEMP VM counter overflow"))
}

fn pager_value(connection: &Connection, sql: &str) -> QueryResult<(u64, u64)> {
    let mut statement = connection.prepare(sql)?;
    let value: i64 = statement.query_row([], |row| row.get(0))?;
    let value = u64::try_from(value)
        .map_err(|_| QueryError::internal("invalid factorized TEMP pager size"))?;
    Ok((value, vm_steps(&statement)?))
}

fn closed_builder() -> QueryError {
    QueryError::internal("factorized builder is closed")
}
fn closed_private() -> QueryError {
    QueryError::internal("factorized TEMP owner is closed")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::Value;
    use crate::query::QueryErrorKind;
    use crate::query::expression::BindingValue;

    fn node(name: &str, id: i64) -> BindingRow {
        let mut row = BindingRow::default();
        row.insert(name.to_owned(), BindingValue::Node(id));
        row
    }

    #[test]
    fn streamed_members_preserve_empty_arms_order_and_transferred_claims() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut builder = FactorTableBuilder::new(node("p", 9), 3, &control).unwrap();
        for id in [2, 2, 3] {
            let row = node("a", id);
            let memory = control
                .reserve(expression::binding_row_bytes_controlled(&row, Some(&control)).unwrap())
                .unwrap();
            builder.push_reserved(1, (row, memory), &control).unwrap();
        }
        let table = builder.finish(&control).unwrap();
        assert_eq!(table.arm_count(), 3);
        assert_eq!(table.arm_len(0), 0);
        assert_eq!(table.arm_len(1), 3);
        assert_eq!(table.prefix().values["p"], BindingValue::Node(9));
        for (ordinal, id) in [2, 2, 3].into_iter().enumerate() {
            let (row, _memory) = table
                .read_arm_row(1, ordinal as u64, &control)
                .unwrap()
                .unwrap();
            assert_eq!(row.values["a"], BindingValue::Node(id));
        }
        assert!(table.read_arm_row(1, 3, &control).unwrap().is_none());
        drop(table);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn bounded_temp_keeps_typed_deltas_and_interleaved_arm_ordinals() {
        let resources = QueryResources::new(Some(1024 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut builder = FactorTableBuilder::new(BindingRow::default(), 2, &control).unwrap();
        let bits = 0x7ff8_0000_0000_0042;
        let mut special = BindingRow::default();
        special.insert(
            "float".into(),
            BindingValue::Scalar(Value::Float(f64::from_bits(bits))),
        );
        let finite_bits = 0x3ff0_0000_0000_0001;
        special.insert(
            "finite".into(),
            BindingValue::Scalar(Value::Float(f64::from_bits(finite_bits))),
        );
        special.insert("null".into(), BindingValue::Null);
        special.used_relationships.insert(77);
        builder.push(0, special, &control).unwrap();
        for id in 0..2000 {
            builder
                .push((id % 2) as usize, node("n", id), &control)
                .unwrap();
        }
        let table = builder.finish(&control).unwrap();
        assert!(table.is_spilled());
        assert_eq!(table.arm_len(0), 1001);
        assert_eq!(table.arm_len(1), 1000);
        let (row, memory) = table.read_arm_row(0, 0, &control).unwrap().unwrap();
        let BindingValue::Scalar(Value::Float(value)) = row.values["float"] else {
            panic!("typed Float was lost")
        };
        // The shared Value JSON contract canonicalizes tagged NaN, while
        // finite Float payload bits remain exact through the spill codec.
        assert!(value.is_nan());
        let BindingValue::Scalar(Value::Float(finite)) = row.values["finite"] else {
            panic!("finite Float was lost")
        };
        assert_eq!(finite.to_bits(), finite_bits);
        assert_eq!(row.values["null"], BindingValue::Null);
        assert!(row.used_relationships.contains(&77));
        drop((row, memory));
        let (row, memory) = table.read_arm_row(1, 999, &control).unwrap().unwrap();
        assert_eq!(row.values["n"], BindingValue::Node(1999));
        let stats = table.storage_metrics();
        assert!(
            stats.spill_encoded_bytes > 0
                && stats.spill_batches > 0
                && stats.spill_vm_steps > 0
                && stats.temp_allocated_bytes > 0
        );
        assert_eq!(stats.memory_peak_bytes, resources.peak());
        drop((row, memory));
        drop(table);
        assert_eq!(resources.used(), 0);
        assert!(resources.peak() <= 1024 * 1024);
    }

    #[test]
    fn cancellation_and_foreign_leases_close_the_unpublished_builder() {
        let resources = QueryResources::default();
        let cancelled = std::cell::Cell::new(false);
        let interrupted = || cancelled.get();
        let control = ExecutionControl::new(&resources, &interrupted);
        let mut builder = FactorTableBuilder::new(BindingRow::default(), 1, &control).unwrap();
        builder.push(0, node("n", 1), &control).unwrap();
        cancelled.set(true);
        assert_eq!(
            builder.push(0, node("n", 2), &control).unwrap_err().kind,
            QueryErrorKind::Interrupted
        );
        assert_eq!(resources.used(), 0);
        assert!(builder.finish(&control).is_err());
        let normal = ExecutionControl::new(&resources, &|| false);
        let mut builder = FactorTableBuilder::new(BindingRow::default(), 1, &normal).unwrap();
        let other = QueryResources::default();
        let foreign = ExecutionControl::new(&other, &|| false);
        let row = node("n", 3);
        let memory = foreign
            .reserve(expression::binding_row_bytes_controlled(&row, Some(&foreign)).unwrap())
            .unwrap();
        assert_eq!(
            builder
                .push_reserved(0, (row, memory), &foreign)
                .unwrap_err()
                .kind,
            QueryErrorKind::Internal
        );
        assert_eq!(resources.used(), 0);
        assert_eq!(other.used(), 0);
    }

    #[test]
    fn private_temp_denial_is_resource_and_the_owner_is_not_retried() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let store = PrivateStore::new(&control).unwrap();
        store
            .apply(|connection| Ok(connection.execute_batch("PRAGMA query_only=ON")?))
            .unwrap();
        let error = store
            .apply(|connection| BindingSpill::create_with_resources(connection, resources.clone()))
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
        assert!(store.connection.borrow().is_none());
        assert!(store.apply(|_| Ok(())).is_err());
        drop(store);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn tight_shared_grant_spills_before_member_growth_spends_encoding_headroom() {
        let resources = QueryResources::new(Some(64 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut builder = FactorTableBuilder::new(BindingRow::default(), 1, &control).unwrap();
        for id in 0..200 {
            let before = resources.used();
            let resident = builder.resident_bytes().unwrap();
            builder.push(0, node("n", id), &control).unwrap_or_else(|error| {
                panic!("id={id} before_used={before} before_resident={resident} peak={} error={error:?}", resources.peak())
            });
        }
        let table = builder.finish(&control).unwrap();
        assert!(table.is_spilled());
        assert_eq!(table.arm_len(0), 200);
        let (row, memory) = table.read_arm_row(0, 199, &control).unwrap().unwrap();
        assert_eq!(row.values["n"], BindingValue::Node(199));
        drop((row, memory));
        drop(table);
        assert_eq!(resources.used(), 0);
        assert!(resources.peak() <= 64 * 1024);
    }

    #[test]
    fn shared_compact_layout_keeps_384_identity_members_below_the_real_soft_target() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut builder = FactorTableBuilder::new(BindingRow::default(), 2, &control).unwrap();
        for id in 0..384 {
            let mut row = node("n", id);
            row.insert(
                "r".into(),
                BindingValue::Relationship(crate::storage::RelationshipRecord {
                    id,
                    source: id,
                    type_id: 1,
                    target: id + 1,
                }),
            );
            row.used_relationships.insert(id);
            builder.push((id % 2) as usize, row, &control).unwrap();
        }
        let table = builder.finish(&control).unwrap();
        assert!(
            !table.is_spilled(),
            "384 real identity deltas must remain compact"
        );
        assert!(table.resident_bytes().unwrap() < FACTOR_RESIDENT_BYTES);
        assert_eq!(table.arm_len(0) + table.arm_len(1), 384);
        let (row, memory) = table.read_arm_row(1, 191, &control).unwrap().unwrap();
        assert_eq!(row.values["n"], BindingValue::Node(383));
        assert_eq!(table.storage_metrics().memory_peak_bytes, resources.peak());
        drop((row, memory));
        drop(table);
        assert_eq!(resources.used(), 0);
    }
}
