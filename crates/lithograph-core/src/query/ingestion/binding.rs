use super::super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, checked_mul,
    map_entry_bytes,
};
use super::*;

pub(crate) fn csv_binding_row(
    input: &BindingRow,
    binding: &str,
    value: Value,
    source: &str,
    line: u64,
    value_memory: MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let mut memory = control.reserve(csv_row_copy_bytes(input, binding, source, control)?)?;
    let mut row = clone_input_row(input, control)?;
    row.insert(
        expression::clone_string_controlled(binding, Some(control))?,
        BindingValue::Scalar(value),
    );
    row.load_csv_context = Some(LoadCsvContext {
        file: Some(expression::clone_string_controlled(source, Some(control))?),
        line: Some(i64::try_from(line).unwrap_or(i64::MAX)),
    });
    memory.merge(value_memory)?;
    memory.resize(expression::binding_row_bytes_controlled(
        &row,
        Some(control),
    )?)?;
    Ok((row, memory))
}

fn csv_row_copy_bytes(
    input: &BindingRow,
    binding: &str,
    source: &str,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let input_bytes = expression::binding_row_bytes_controlled(input, Some(control))?;
    let names = checked_add(source.len() as u64, checked_mul(binding.len() as u64, 2)?)?;
    let entry = checked_add(
        map_entry_bytes::<BindingValue>()?,
        allocation_bytes::<String>(1)?,
    )?;
    checked_add(input_bytes, checked_add(names, entry)?)
}

fn clone_input_row(input: &BindingRow, control: &ExecutionControl<'_>) -> QueryResult<BindingRow> {
    let mut row = BindingRow::default();
    for (name, value) in &input.values {
        control.work(1)?;
        row.values.insert(
            expression::clone_string_controlled(name, Some(control))?,
            clone_binding_value(value, control)?,
        );
    }
    let order_capacity = input.order.len().checked_add(1).ok_or_else(|| {
        super::super::resources::memory_error("LOAD CSV binding order capacity overflow")
    })?;
    row.order = Vec::with_capacity(order_capacity);
    for name in &input.order {
        control.work(1)?;
        row.order
            .push(expression::clone_string_controlled(name, Some(control))?);
    }
    for id in &input.used_relationships {
        control.work(1)?;
        row.used_relationships.insert(*id);
    }
    Ok(row)
}

fn clone_binding_value(
    value: &BindingValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<BindingValue> {
    Ok(match value {
        BindingValue::Scalar(value) => BindingValue::Scalar(
            expression::clone_value_unreserved_controlled(value, Some(control))?,
        ),
        BindingValue::Path {
            nodes,
            relationships,
        } => BindingValue::Path {
            nodes: clone_records(nodes, control)?,
            relationships: clone_records(relationships, control)?,
        },
        BindingValue::Node(id) => BindingValue::Node(*id),
        BindingValue::Relationship(record) => BindingValue::Relationship(*record),
        BindingValue::Null => BindingValue::Null,
    })
}

fn clone_records<T: Copy>(values: &[T], control: &ExecutionControl<'_>) -> QueryResult<Vec<T>> {
    let mut output = Vec::with_capacity(values.len());
    for chunk in values.chunks(256) {
        control.work(chunk.len() as u64)?;
        output.extend_from_slice(chunk);
    }
    Ok(output)
}
