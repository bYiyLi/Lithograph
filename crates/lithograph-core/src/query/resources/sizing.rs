use std::collections::BTreeMap;
use std::mem::size_of;

use serde_json::Value as JsonValue;

use super::{ExecutionControl, QueryResult, memory_error};
use crate::cypher::{NodeValue, RelationshipValue, Value, VectorValues};

pub fn checked_add(left: u64, right: u64) -> QueryResult<u64> {
    left.checked_add(right)
        .ok_or_else(|| memory_error("query execution memory size overflow"))
}

pub fn checked_mul(left: u64, right: u64) -> QueryResult<u64> {
    left.checked_mul(right)
        .ok_or_else(|| memory_error("query execution memory size overflow"))
}

pub fn allocation_bytes<T>(capacity: usize) -> QueryResult<u64> {
    checked_mul(capacity as u64, size_of::<T>() as u64)
}

/// BTreeMap hides node capacity, and even a singleton owns a whole leaf. Allow
/// sixteen inline entry slots and a header/child-pointer allowance per entry;
/// this deliberately overcounts shared nodes instead of undercounting sparse
/// maps. Owned key and value heaps are additional to this node allowance.
pub fn map_entry_bytes<T>() -> QueryResult<u64> {
    checked_add(checked_mul(16, size_of::<(String, T)>() as u64)?, 256)
}

/// Includes the value's inline storage and its owned heap allocations. A caller
/// charging a containing Vec uses `row_bytes` to count inline slots once.
pub fn value_bytes(value: &Value) -> QueryResult<u64> {
    value_bytes_inner(value, None)
}

pub fn value_bytes_controlled(value: &Value, control: &ExecutionControl<'_>) -> QueryResult<u64> {
    value_bytes_inner(value, Some(control))
}

fn value_bytes_inner(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    checked_add(size_of::<Value>() as u64, value_heap_bytes(value, control)?)
}

pub fn estimate_value_bytes(value: &Value) -> QueryResult<u64> {
    value_bytes(value)
}

pub fn row_bytes(values: &[Value]) -> QueryResult<u64> {
    row_bytes_inner(values, None)
}

/// Counts an owning row's spare slots as well as its populated values. A slice
/// alone cannot describe retained Vec capacity and must not release that claim.
pub fn owned_row_bytes(values: &Vec<Value>) -> QueryResult<u64> {
    checked_add(
        row_bytes(values)?,
        allocation_bytes::<Value>(values.capacity() - values.len())?,
    )
}

pub fn owned_row_bytes_controlled(
    values: &Vec<Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    checked_add(
        row_bytes_controlled(values, control)?,
        allocation_bytes::<Value>(values.capacity() - values.len())?,
    )
}

pub fn row_bytes_controlled(values: &[Value], control: &ExecutionControl<'_>) -> QueryResult<u64> {
    row_bytes_inner(values, Some(control))
}

fn row_bytes_inner(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    values
        .iter()
        .try_fold(allocation_bytes::<Value>(values.len())?, |bytes, value| {
            checked_add(bytes, value_heap_bytes(value, control)?)
        })
}

pub fn json_bytes(value: &JsonValue) -> QueryResult<u64> {
    json_bytes_inner(value, None)
}

pub fn json_bytes_controlled(
    value: &JsonValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    json_bytes_inner(value, Some(control))
}

fn json_bytes_inner(value: &JsonValue, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    checked_add(
        size_of::<JsonValue>() as u64,
        json_heap_bytes(value, control)?,
    )
}

fn tick(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    if let Some(control) = control {
        control.work(1)?;
    }
    Ok(())
}

fn value_heap_bytes(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    tick(control)?;
    match value {
        Value::String(text) => Ok(text.capacity() as u64),
        Value::List(values) => values.iter().try_fold(
            allocation_bytes::<Value>(values.capacity())?,
            |bytes, value| checked_add(bytes, value_heap_bytes(value, control)?),
        ),
        Value::Map(values) => map_bytes(values, control),
        Value::Node(node) => node_heap_bytes(node, control),
        Value::Relationship(relationship) => relationship_heap_bytes(relationship, control),
        Value::Path(path) => {
            let mut bytes = checked_add(
                allocation_bytes::<NodeValue>(path.nodes.capacity())?,
                allocation_bytes::<RelationshipValue>(path.relationships.capacity())?,
            )?;
            for node in &path.nodes {
                bytes = checked_add(bytes, node_heap_bytes(node, control)?)?;
            }
            for relationship in &path.relationships {
                bytes = checked_add(bytes, relationship_heap_bytes(relationship, control)?)?;
            }
            Ok(bytes)
        }
        _ => scalar_heap_bytes(value),
    }
}

fn scalar_heap_bytes(value: &Value) -> QueryResult<u64> {
    match value {
        Value::Date(value) => Ok(value.accounted_heap_bytes() as u64),
        Value::LocalTime(value) => Ok(value.accounted_heap_bytes() as u64),
        Value::Time(value) => Ok(value.accounted_heap_bytes() as u64),
        Value::LocalDateTime(value) => Ok(value.accounted_heap_bytes() as u64),
        Value::ZonedDateTime(value) => Ok(value.accounted_heap_bytes() as u64),
        Value::Duration(value) => Ok(value.accounted_heap_bytes() as u64),
        Value::Point(value) => Ok(value.accounted_heap_bytes() as u64),
        Value::Vector(value) => vector_bytes(value.values()),
        _ => Ok(0),
    }
}

fn node_heap_bytes(node: &NodeValue, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    tick(control)?;
    let labels = node.labels.iter().try_fold(
        allocation_bytes::<String>(node.labels.capacity())?,
        |bytes, text| {
            tick(control)?;
            checked_add(bytes, text.capacity() as u64)
        },
    )?;
    checked_add(
        checked_add(node.element_id.capacity() as u64, labels)?,
        map_bytes(&node.properties, control)?,
    )
}

fn relationship_heap_bytes(
    value: &RelationshipValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    tick(control)?;
    let strings = [
        &value.element_id,
        &value.relationship_type,
        &value.start,
        &value.end,
    ]
    .iter()
    .try_fold(0, |bytes, text| checked_add(bytes, text.capacity() as u64))?;
    checked_add(strings, map_bytes(&value.properties, control)?)
}

fn map_bytes(
    values: &BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    values.iter().try_fold(0, |bytes, (key, value)| {
        let inline = map_entry_bytes::<Value>()?;
        checked_add(
            bytes,
            checked_add(
                checked_add(inline, key.capacity() as u64)?,
                value_heap_bytes(value, control)?,
            )?,
        )
    })
}

fn vector_bytes(values: &VectorValues) -> QueryResult<u64> {
    match values {
        VectorValues::I8(values) => allocation_bytes::<i8>(values.capacity()),
        VectorValues::I16(values) => allocation_bytes::<i16>(values.capacity()),
        VectorValues::I32(values) => allocation_bytes::<i32>(values.capacity()),
        VectorValues::I64(values) => allocation_bytes::<i64>(values.capacity()),
        VectorValues::F32(values) => allocation_bytes::<f32>(values.capacity()),
        VectorValues::F64(values) => allocation_bytes::<f64>(values.capacity()),
    }
}

fn json_heap_bytes(value: &JsonValue, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    tick(control)?;
    match value {
        JsonValue::String(text) => Ok(text.capacity() as u64),
        JsonValue::Array(values) => values.iter().try_fold(
            allocation_bytes::<JsonValue>(values.capacity())?,
            |bytes, value| checked_add(bytes, json_heap_bytes(value, control)?),
        ),
        JsonValue::Object(values) => values.iter().try_fold(0, |bytes, (key, value)| {
            let inline = map_entry_bytes::<JsonValue>()?;
            checked_add(
                bytes,
                checked_add(
                    checked_add(inline, key.capacity() as u64)?,
                    json_heap_bytes(value, control)?,
                )?,
            )
        }),
        _ => Ok(0),
    }
}
