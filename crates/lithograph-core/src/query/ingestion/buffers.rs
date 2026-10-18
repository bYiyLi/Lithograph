use super::super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, memory_error,
};
use super::QueryResult;

pub(super) fn reserve_vector<T>(
    values: &mut Vec<T>,
    additional: usize,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let needed = values
        .len()
        .checked_add(additional)
        .ok_or_else(|| memory_error("LOAD CSV buffer size overflow"))?;
    if needed <= values.capacity() {
        return Ok(());
    }
    let capacity = next_capacity(values.capacity(), needed)?;
    let replacement_memory = control.reserve(allocation_bytes::<T>(capacity)?)?;
    let mut replacement = Vec::new();
    replacement
        .try_reserve_exact(capacity)
        .map_err(|_| memory_error("unable to allocate LOAD CSV buffer"))?;
    for value in values.drain(..) {
        control.work(1)?;
        replacement.push(value);
    }
    let previous = std::mem::replace(values, replacement);
    let previous_bytes = allocation_bytes::<T>(previous.capacity())?;
    drop(previous);
    drop(memory.split(previous_bytes)?);
    memory.merge(replacement_memory)
}

pub(super) fn reserve_string(
    value: &mut String,
    additional: usize,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    let needed = value
        .len()
        .checked_add(additional)
        .ok_or_else(|| memory_error("LOAD CSV field size overflow"))?;
    if needed <= value.capacity() {
        return Ok(());
    }
    let capacity = next_capacity(value.capacity(), needed)?;
    let replacement_memory = control.reserve(capacity as u64)?;
    let mut replacement = String::new();
    replacement
        .try_reserve_exact(capacity)
        .map_err(|_| memory_error("unable to allocate LOAD CSV field"))?;
    let mut start = 0;
    while start < value.len() {
        let mut end = (start + 64 * 1024).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        control.bytes((end - start) as u64)?;
        replacement.push_str(&value[start..end]);
        start = end;
    }
    let previous = std::mem::replace(value, replacement);
    let previous_bytes = previous.capacity() as u64;
    drop(previous);
    drop(memory.split(previous_bytes)?);
    memory.merge(replacement_memory)
}

fn next_capacity(current: usize, needed: usize) -> QueryResult<usize> {
    let doubled = checked_add(current as u64, current as u64)?;
    usize::try_from(doubled.max(needed as u64).max(8))
        .map_err(|_| memory_error("LOAD CSV capacity exceeds usize"))
}
