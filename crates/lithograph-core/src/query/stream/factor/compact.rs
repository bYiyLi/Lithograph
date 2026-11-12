//! Shared arm columns with lossless, moved typed cells. The existing Binding
//! codec is used only when a consumer asks for one ordinary row.

use std::mem::size_of;

use crate::query::expression::{self, BindingRow, BindingValue, LoadCsvContext};
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};
use crate::query::{
    ExecutionControl, MemoryReservation, QueryError, QueryErrorKind, QueryResources, QueryResult,
};

#[derive(Debug)]
pub(crate) struct CompactLayout {
    names: Vec<String>,
    order: Vec<usize>,
    memory: MemoryReservation,
    resources: QueryResources,
}

#[derive(Debug)]
pub(crate) struct CompactMember {
    values: Vec<BindingValue>,
    used_relationships: Vec<i64>,
    csv: Option<LoadCsvContext>,
    memory: MemoryReservation,
}

#[derive(Debug)]
pub(crate) enum PackResult {
    Packed(CompactMember),
    Fallback((BindingRow, MemoryReservation)),
}

struct MemberBuffers {
    values: Vec<BindingValue>,
    ids: Vec<i64>,
    memory: MemoryReservation,
}

#[cfg(test)]
pub(crate) fn compact_member_slot_bytes() -> u64 {
    size_of::<CompactMember>() as u64
}

pub(crate) fn compact_cell_bytes() -> u64 {
    size_of::<BindingValue>() as u64
}

pub(crate) fn compact_name_slot_bytes() -> u64 {
    size_of::<String>() as u64
}

pub(crate) fn compact_layout_shape_bytes(
    columns: u64,
    order: u64,
    name_bytes: u64,
) -> QueryResult<u64> {
    checked_add(
        checked_add(
            checked_mul(columns, compact_name_slot_bytes())?,
            checked_mul(order, size_of::<usize>() as u64)?,
        )?,
        name_bytes,
    )
}

pub(crate) fn compact_member_heap_bytes(
    cells: u64,
    used_relationships: u64,
    value_heap: u64,
) -> QueryResult<u64> {
    checked_add(
        checked_add(
            checked_mul(cells, compact_cell_bytes())?,
            checked_mul(used_relationships, size_of::<i64>() as u64)?,
        )?,
        value_heap,
    )
}

pub(crate) fn compact_layout_bytes(
    row: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let mut bytes = 0;
    for name in row.values.keys() {
        control.work(1)?;
        bytes = checked_add(bytes, name.len() as u64)?;
    }
    compact_layout_shape_bytes(row.values.len() as u64, row.order.len() as u64, bytes)
}

impl CompactLayout {
    pub(crate) fn try_new(
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<Self>> {
        control.check()?;
        if row.order.iter().any(|name| !row.values.contains_key(name)) {
            return Ok(None);
        }
        let Some(mut memory) = optional_claim(compact_layout_bytes(row, control)?, control)? else {
            return Ok(None);
        };
        let mut names = Vec::new();
        let mut order = Vec::new();
        if names.try_reserve_exact(row.values.len()).is_err()
            || order.try_reserve_exact(row.order.len()).is_err()
        {
            return Ok(None);
        }
        for name in row.values.keys() {
            names.push(expression::clone_string_controlled(name, Some(control))?);
        }
        for name in &row.order {
            control.work(1)?;
            order.push(names.binary_search(name).map_err(|_| invalid_layout())?);
        }
        let bytes = layout_allocated_bytes(&names, names.capacity(), order.capacity())?;
        if !optional_resize(&mut memory, bytes, control)? {
            return Ok(None);
        }
        Ok(Some(Self {
            names,
            order,
            memory,
            resources: control.resources().clone(),
        }))
    }

    pub(crate) fn owned_bytes(&self) -> u64 {
        self.memory.bytes()
    }

    pub(crate) fn pack(
        &self,
        input: (BindingRow, MemoryReservation),
        control: &ExecutionControl<'_>,
    ) -> QueryResult<PackResult> {
        control.check()?;
        self.check_owner(control)?;
        let (row, mut memory) = input;
        memory.merge(control.reserve(0)?)?;
        if memory.bytes() < expression::binding_row_bytes_controlled(&row, Some(control))? {
            return Err(QueryError::internal("compact input is not fully reserved"));
        }
        if !self.matches(&row, control)? {
            return Ok(PackResult::Fallback((row, memory)));
        }
        let Some(buffers) = member_buffers(&row, control)? else {
            return Ok(PackResult::Fallback((row, memory)));
        };
        finish_pack(row, memory, buffers, control).map(PackResult::Packed)
    }

    fn matches(&self, row: &BindingRow, control: &ExecutionControl<'_>) -> QueryResult<bool> {
        if self.names.len() != row.values.len() || self.order.len() != row.order.len() {
            return Ok(false);
        }
        for (expected, actual) in self.names.iter().zip(row.values.keys()) {
            if !same_name(expected, actual, control)? {
                return Ok(false);
            }
        }
        for (position, actual) in self.order.iter().zip(&row.order) {
            if !same_name(&self.names[*position], actual, control)? {
                return Ok(false);
            }
        }
        Ok(true)
    }

    #[cfg(test)]
    pub(crate) fn read(
        &self,
        member: &CompactMember,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(BindingRow, MemoryReservation)> {
        self.read_measured(member, control)
            .map(|(row, memory, _)| (row, memory))
    }

    pub(crate) fn read_measured(
        &self,
        member: &CompactMember,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<(BindingRow, MemoryReservation, u64)> {
        control.check()?;
        self.check_owner(control)?;
        if member.values.len() != self.names.len() {
            return Err(invalid_layout());
        }
        let mut memory = control.reserve(self.expanded_bytes(member, control)?)?;
        let initial_claim = memory.bytes();
        let row = self.expanded_row(member, control)?;
        memory.resize(expression::binding_row_bytes_controlled(
            &row,
            Some(control),
        )?)?;
        let peak_claim = initial_claim.max(memory.bytes());
        Ok((row, memory, peak_claim))
    }

    fn expanded_row(
        &self,
        member: &CompactMember,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<BindingRow> {
        let mut row = BindingRow::default();
        for (name, value) in self.names.iter().zip(&member.values) {
            row.values.insert(
                expression::clone_string_controlled(name, Some(control))?,
                expression::clone_binding_value_unreserved_controlled(value, Some(control))?,
            );
        }
        row.order.try_reserve_exact(self.order.len()).map_err(|_| {
            crate::query::resources::memory_error("unable to allocate compact row order")
        })?;
        for position in &self.order {
            row.order.push(expression::clone_string_controlled(
                &self.names[*position],
                Some(control),
            )?);
        }
        for id in &member.used_relationships {
            control.work(1)?;
            row.used_relationships.insert(*id);
        }
        row.load_csv_context =
            expression::clone_load_csv_context_controlled(&member.csv, Some(control))?;
        Ok(row)
    }

    fn expanded_bytes(
        &self,
        member: &CompactMember,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<u64> {
        let mut bytes = checked_add(
            size_of::<BindingRow>() as u64,
            allocation_bytes::<String>(self.order.len())?,
        )?;
        for (name, value) in self.names.iter().zip(&member.values) {
            bytes = checked_add(
                bytes,
                checked_add(map_entry_bytes::<BindingValue>()?, name.len() as u64)?,
            )?;
            bytes = checked_add(
                bytes,
                expression::binding_value_bytes(value, Some(control))?,
            )?;
        }
        for position in &self.order {
            control.work(1)?;
            bytes = checked_add(bytes, self.names[*position].len() as u64)?;
        }
        bytes = checked_add(
            bytes,
            checked_mul(
                member.used_relationships.len() as u64,
                map_entry_bytes::<i64>()?,
            )?,
        )?;
        checked_add(bytes, csv_heap_bytes(&member.csv))
    }

    fn check_owner(&self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        let mut owner = self.resources.reserve(0)?;
        owner.merge(control.reserve(0)?)
    }
}

impl CompactMember {
    pub(crate) fn owned_bytes(&self) -> u64 {
        self.memory.bytes()
    }

    pub(crate) fn buffer_bytes(&self) -> QueryResult<u64> {
        checked_add(
            allocation_bytes::<BindingValue>(self.values.capacity())?,
            allocation_bytes::<i64>(self.used_relationships.capacity())?,
        )
    }

    fn heap_bytes(&self, control: &ExecutionControl<'_>) -> QueryResult<u64> {
        let mut bytes = self.buffer_bytes()?;
        for value in &self.values {
            bytes = checked_add(bytes, value_heap_bytes(value, control)?)?;
        }
        checked_add(bytes, csv_heap_bytes(&self.csv))
    }
}

fn member_buffers(
    row: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<MemberBuffers>> {
    let bytes = checked_add(
        allocation_bytes::<BindingValue>(row.values.len())?,
        allocation_bytes::<i64>(row.used_relationships.len())?,
    )?;
    let Some(mut memory) = optional_claim(bytes, control)? else {
        return Ok(None);
    };
    let mut values = Vec::new();
    let mut ids = Vec::new();
    if values.try_reserve_exact(row.values.len()).is_err()
        || ids.try_reserve_exact(row.used_relationships.len()).is_err()
    {
        return Ok(None);
    }
    let bytes = checked_add(
        allocation_bytes::<BindingValue>(values.capacity())?,
        allocation_bytes::<i64>(ids.capacity())?,
    )?;
    if !optional_resize(&mut memory, bytes, control)? {
        return Ok(None);
    }
    Ok(Some(MemberBuffers {
        values,
        ids,
        memory,
    }))
}

fn finish_pack(
    row: BindingRow,
    mut memory: MemoryReservation,
    mut buffers: MemberBuffers,
    control: &ExecutionControl<'_>,
) -> QueryResult<CompactMember> {
    let BindingRow {
        values,
        used_relationships,
        order,
        load_csv_context,
    } = row;
    for (_, value) in values {
        control.work(1)?;
        buffers.values.push(value);
    }
    for id in used_relationships {
        control.work(1)?;
        buffers.ids.push(id);
    }
    drop(order);
    memory.merge(buffers.memory)?;
    let mut member = CompactMember {
        values: buffers.values,
        used_relationships: buffers.ids,
        csv: load_csv_context,
        memory,
    };
    let bytes = member.heap_bytes(control)?;
    member.memory.resize(bytes)?;
    Ok(member)
}

fn optional_claim(
    bytes: u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<MemoryReservation>> {
    control.check()?;
    match control.resources().reserve(bytes) {
        Ok(memory) => Ok(Some(memory)),
        Err(error) if error.kind == QueryErrorKind::Resource => Ok(None),
        Err(error) => Err(error),
    }
}

fn optional_resize(
    memory: &mut MemoryReservation,
    bytes: u64,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    control.check()?;
    match memory.resize(bytes) {
        Ok(()) => Ok(true),
        Err(error) if error.kind == QueryErrorKind::Resource => Ok(false),
        Err(error) => Err(error),
    }
}

fn value_heap_bytes(value: &BindingValue, control: &ExecutionControl<'_>) -> QueryResult<u64> {
    let bytes = expression::binding_value_bytes(value, Some(control))?;
    Ok(if matches!(value, BindingValue::Scalar(_)) {
        bytes.saturating_sub(size_of::<crate::cypher::Value>() as u64)
    } else {
        bytes
    })
}

fn csv_heap_bytes(csv: &Option<LoadCsvContext>) -> u64 {
    csv.as_ref()
        .and_then(|csv| csv.file.as_ref())
        .map_or(0, |file| file.capacity() as u64)
}

fn layout_allocated_bytes(
    names: &[String],
    names_capacity: usize,
    order_capacity: usize,
) -> QueryResult<u64> {
    let mut bytes = checked_add(
        allocation_bytes::<String>(names_capacity)?,
        allocation_bytes::<usize>(order_capacity)?,
    )?;
    for name in names {
        bytes = checked_add(bytes, name.capacity() as u64)?;
    }
    Ok(bytes)
}

fn same_name(expected: &str, actual: &str, control: &ExecutionControl<'_>) -> QueryResult<bool> {
    control.work(1)?;
    control.bytes(expected.len().saturating_add(actual.len()) as u64)?;
    Ok(expected == actual)
}

fn invalid_layout() -> QueryError {
    QueryError::internal("compact factor layout disagrees with its member")
}

#[cfg(test)]
mod tests;
