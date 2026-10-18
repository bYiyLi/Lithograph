//! Pre-allocation bounds for the existing Lithograph JSON value encoding.

use std::collections::BTreeMap;
use std::mem::size_of;

use serde_json::Value as JsonValue;

use super::{
    ExecutionControl, QueryResult, allocation_bytes, checked_add, checked_mul, map_entry_bytes,
};
use crate::cypher::{NodeValue, RelationshipValue, Value, VectorValues};

const INLINE: u64 = size_of::<JsonValue>() as u64;
const JS_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

/// Bounds the resulting JSON tree before it is constructed, including tagged
/// values. Source values and encoder temporaries remain separate live owners.
pub fn encoded_json_bytes(value: &Value) -> QueryResult<u64> {
    encoded_inner(value, None)
}

pub fn encoded_json_bytes_controlled(
    value: &Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    encoded_inner(value, Some(control))
}

fn encoded_inner(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    tick(control)?;
    match value {
        Value::String(text) => string(text),
        Value::List(values) => array(
            values.len(),
            values.iter().map(|value| encoded_inner(value, control)),
            control,
        ),
        Value::Map(values) => encoded_map(values, true, control),
        Value::Node(value) => node(value, control),
        Value::Relationship(value) => relationship(value, control),
        Value::Path(value) => object(&[
            ("$type", string("Path")?),
            (
                "nodes",
                array(
                    value.nodes.len(),
                    value.nodes.iter().map(|value| node(value, control)),
                    control,
                )?,
            ),
            (
                "relationships",
                array(
                    value.relationships.len(),
                    value
                        .relationships
                        .iter()
                        .map(|value| relationship(value, control)),
                    control,
                )?,
            ),
        ]),
        Value::Vector(value) => vector(value.values(), control),
        Value::Point(value) => object(&[
            ("$type", string("Point")?),
            ("crs", string(value.crs())?),
            (
                "coordinates",
                array(
                    value.coordinates().len(),
                    value.coordinates().iter().map(|_| Ok(INLINE)),
                    control,
                )?,
            ),
        ]),
        _ => scalar(value),
    }
}

pub fn encoded_row_bytes(values: &[Value]) -> QueryResult<u64> {
    array(values.len(), values.iter().map(encoded_json_bytes), None)
}

pub fn encoded_row_bytes_controlled(
    values: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    array(
        values.len(),
        values
            .iter()
            .map(|value| encoded_inner(value, Some(control))),
        Some(control),
    )
}

fn scalar(value: &Value) -> QueryResult<u64> {
    match value {
        Value::Integer(value) if !(-JS_SAFE_INTEGER..=JS_SAFE_INTEGER).contains(value) => {
            // Decimal i64 text needs at most twenty bytes including its sign.
            tagged("Integer", 20)
        }
        Value::Float(value) if !value.is_finite() => tagged("Float", 9),
        Value::Date(value) => tagged("Date", value.as_str().len()),
        Value::LocalTime(value) => tagged("LocalTime", value.as_str().len()),
        Value::Time(value) => tagged("Time", value.as_str().len()),
        Value::LocalDateTime(value) => tagged("LocalDateTime", value.as_str().len()),
        Value::ZonedDateTime(value) => object(&[
            ("$type", string("ZonedDateTime")?),
            ("value", string(value.value())?),
            ("zone", string(value.zone())?),
        ]),
        Value::Duration(value) => tagged("Duration", value.as_str().len()),
        Value::Uuid(_) => tagged("UUID", 36),
        _ => Ok(INLINE),
    }
}

fn string(value: &str) -> QueryResult<u64> {
    string_len(value.len())
}

fn string_len(len: usize) -> QueryResult<u64> {
    checked_add(INLINE, checked_mul((len as u64).max(8), 2)?)
}

fn tagged(tag: &str, text_len: usize) -> QueryResult<u64> {
    object(&[("$type", string(tag)?), ("value", string_len(text_len)?)])
}

fn tick(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    if let Some(control) = control {
        control.work(1)?;
    }
    Ok(())
}

fn array(
    len: usize,
    mut elements: impl Iterator<Item = QueryResult<u64>>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    let capacity = if len == 0 { 0 } else { len.max(4) };
    let container = checked_add(
        INLINE,
        checked_mul(2, allocation_bytes::<JsonValue>(capacity)?)?,
    )?;
    // Including each child's inline slot again is intentional conservatism.
    elements.try_fold(container, |bytes, element| {
        tick(control)?;
        checked_add(bytes, element?)
    })
}

fn object(fields: &[(&str, u64)]) -> QueryResult<u64> {
    fields.iter().try_fold(INLINE, |bytes, (key, value)| {
        checked_add(bytes, entry(key, *value)?)
    })
}

fn entry(key: &str, value_bytes: u64) -> QueryResult<u64> {
    checked_add(
        map_entry_bytes::<JsonValue>()?,
        checked_add(checked_mul((key.len() as u64).max(8), 2)?, value_bytes)?,
    )
}

fn encoded_map(
    values: &BTreeMap<String, Value>,
    wrap: bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    let entries = values.iter().try_fold(INLINE, |bytes, (key, value)| {
        checked_add(bytes, entry(key, encoded_inner(value, control)?)?)
    })?;
    if wrap && values.contains_key("$type") {
        object(&[("$type", string("Map")?), ("entries", entries)])
    } else {
        Ok(entries)
    }
}

fn node(value: &NodeValue, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    tick(control)?;
    object(&[
        ("$type", string("Node")?),
        ("elementId", string(&value.element_id)?),
        (
            "labels",
            array(
                value.labels.len(),
                value.labels.iter().map(|value| string(value)),
                control,
            )?,
        ),
        (
            "properties",
            encoded_map(&value.properties, false, control)?,
        ),
    ])
}

fn relationship(
    value: &RelationshipValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    tick(control)?;
    object(&[
        ("$type", string("Relationship")?),
        ("elementId", string(&value.element_id)?),
        ("type", string(&value.relationship_type)?),
        ("start", string(&value.start)?),
        ("end", string(&value.end)?),
        (
            "properties",
            encoded_map(&value.properties, false, control)?,
        ),
    ])
}

fn vector(values: &VectorValues, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    let numeric = match values {
        VectorValues::I64(values) => array(
            values.len(),
            values.iter().map(|value| scalar(&Value::Integer(*value))),
            control,
        )?,
        _ => array(values.len(), (0..values.len()).map(|_| Ok(INLINE)), control)?,
    };
    object(&[
        ("$type", string("Vector")?),
        ("coordinateType", string_len(3)?),
        ("dimension", INLINE),
        ("values", numeric),
    ])
}
