use crate::cypher::Value;

use super::QueryResult;
use super::VersionExecution;

/// Version state equality follows frozen LCE1 identity, not Cypher numeric
/// equality: all NaN payloads denote one state, while signed zeros differ.
pub(super) fn same_optional_value_controlled(
    connection: &VersionExecution<'_, '_>,
    left: Option<&Value>,
    right: Option<&Value>,
) -> QueryResult<bool> {
    match (left, right) {
        (Some(left), Some(right)) => same_value_controlled(connection, left, right),
        (None, None) => Ok(true),
        _ => Ok(false),
    }
}

pub(super) fn same_value_controlled(
    connection: &VersionExecution<'_, '_>,
    left: &Value,
    right: &Value,
) -> QueryResult<bool> {
    connection.checkpoint()?;
    match (left, right) {
        (Value::String(left), Value::String(right)) => {
            same_bytes(connection, left.as_bytes(), right.as_bytes())
        }
        (Value::Float(left), Value::Float(right)) => Ok(same_float(*left, *right)),
        (Value::List(left), Value::List(right)) => {
            if left.len() != right.len() {
                return Ok(false);
            }
            for (left, right) in left.iter().zip(right) {
                if !same_value_controlled(connection, left, right)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        (Value::Map(left), Value::Map(right)) => same_maps(connection, left, right),
        (Value::Point(left), Value::Point(right)) => {
            if left.crs() != right.crs() || left.srid() != right.srid() {
                return Ok(false);
            }
            same_float_coordinates(connection, left.coordinates(), right.coordinates())
        }
        (Value::Vector(left), Value::Vector(right)) => same_vectors(connection, left, right),
        _ => Ok(left == right),
    }
}

fn same_maps(
    connection: &VersionExecution<'_, '_>,
    left: &std::collections::BTreeMap<String, Value>,
    right: &std::collections::BTreeMap<String, Value>,
) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for ((key, left), (other, right)) in left.iter().zip(right) {
        if !same_bytes(connection, key.as_bytes(), other.as_bytes())?
            || !same_value_controlled(connection, left, right)?
        {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(super) fn same_bytes(
    connection: &VersionExecution<'_, '_>,
    left: &[u8],
    right: &[u8],
) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.chunks(64 * 1024).zip(right.chunks(64 * 1024)) {
        connection.control().bytes(left.len() as u64)?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

fn same_float(left: f64, right: f64) -> bool {
    (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
}

fn same_float_coordinates(
    connection: &VersionExecution<'_, '_>,
    left: &[f64],
    right: &[f64],
) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.iter().zip(right) {
        connection.checkpoint()?;
        if !same_float(*left, *right) {
            return Ok(false);
        }
    }
    Ok(true)
}

fn same_vectors(
    connection: &VersionExecution<'_, '_>,
    left: &crate::cypher::VectorValue,
    right: &crate::cypher::VectorValue,
) -> QueryResult<bool> {
    use crate::cypher::VectorValues as V;
    if left.coordinate_type() != right.coordinate_type() {
        return Ok(false);
    }
    match (left.values(), right.values()) {
        (V::I8(left), V::I8(right)) => same_integer_coordinates(connection, left, right),
        (V::I16(left), V::I16(right)) => same_integer_coordinates(connection, left, right),
        (V::I32(left), V::I32(right)) => same_integer_coordinates(connection, left, right),
        (V::I64(left), V::I64(right)) => same_integer_coordinates(connection, left, right),
        (V::F32(left), V::F32(right)) => same_f32_coordinates(connection, left, right),
        (V::F64(left), V::F64(right)) => same_float_coordinates(connection, left, right),
        _ => Ok(false),
    }
}

fn same_integer_coordinates<T: PartialEq>(
    connection: &VersionExecution<'_, '_>,
    left: &[T],
    right: &[T],
) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.iter().zip(right) {
        connection.checkpoint()?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

fn same_f32_coordinates(
    connection: &VersionExecution<'_, '_>,
    left: &[f32],
    right: &[f32],
) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.iter().zip(right) {
        connection.checkpoint()?;
        if !(left.is_nan() && right.is_nan()) && left.to_bits() != right.to_bits() {
            return Ok(false);
        }
    }
    Ok(true)
}
