use crate::storage::{self, PropertyValue, VectorCoordinateType};

use super::super::QueryResult;

pub(crate) fn property_equality_key(value: &PropertyValue) -> QueryResult<Option<Vec<u8>>> {
    match value {
        PropertyValue::Integer(value) => Ok(Some(numeric_key(*value))),
        PropertyValue::Float(value) if value.is_nan() => Ok(None),
        value @ PropertyValue::Float(number) => match float_as_exact_integer(*number) {
            Some(number) => Ok(Some(numeric_key(number))),
            None => typed_key(value).map(Some),
        },
        PropertyValue::List(values) => list_key(values),
        PropertyValue::Point(point) => point_key(point),
        PropertyValue::Vector(vector) => vector_key(vector),
        value => typed_key(value).map(Some),
    }
}

fn numeric_key(value: i64) -> Vec<u8> {
    let mut key = b"number".to_vec();
    key.extend_from_slice(&value.to_le_bytes());
    key
}

fn typed_key(value: &PropertyValue) -> QueryResult<Vec<u8>> {
    let canonical = value.canonical_bytes()?;
    let mut key = b"typed".to_vec();
    key.extend_from_slice(&canonical);
    Ok(key)
}

fn list_key(values: &[PropertyValue]) -> QueryResult<Option<Vec<u8>>> {
    let mut key = b"list".to_vec();
    for value in values {
        let Some(value_key) = property_equality_key(value)? else {
            return Ok(None);
        };
        key.extend_from_slice(&(value_key.len() as u64).to_le_bytes());
        key.extend_from_slice(&value_key);
    }
    Ok(Some(key))
}

fn point_key(point: &storage::PointValue) -> QueryResult<Option<Vec<u8>>> {
    let mut key = b"point".to_vec();
    key.extend_from_slice(&point.crs.to_le_bytes());
    key.extend_from_slice(&(point.coordinates.len() as u64).to_le_bytes());
    for coordinate in &point.coordinates {
        let Some(bits) = equality_f64_bits(*coordinate) else {
            return Ok(None);
        };
        key.extend_from_slice(&bits.to_le_bytes());
    }
    Ok(Some(key))
}

fn vector_key(vector: &storage::VectorValue) -> QueryResult<Option<Vec<u8>>> {
    let value = PropertyValue::Vector(vector.clone());
    let _ = value.canonical_bytes()?;
    let mut key = b"vector".to_vec();
    key.push(vector.coordinate_type as u8);
    key.extend_from_slice(&vector.dimension.to_le_bytes());
    match vector.coordinate_type {
        VectorCoordinateType::F32 => {
            for chunk in vector.packed.as_chunks::<4>().0 {
                let value = f32::from_le_bytes(*chunk);
                if value.is_nan() {
                    return Ok(None);
                }
                let bits = if value == 0.0 { 0 } else { value.to_bits() };
                key.extend_from_slice(&bits.to_le_bytes());
            }
        }
        VectorCoordinateType::F64 => {
            for chunk in vector.packed.as_chunks::<8>().0 {
                let value = f64::from_le_bytes(*chunk);
                let Some(bits) = equality_f64_bits(value) else {
                    return Ok(None);
                };
                key.extend_from_slice(&bits.to_le_bytes());
            }
        }
        _ => return typed_key(&value).map(Some),
    }
    Ok(Some(key))
}

fn equality_f64_bits(value: f64) -> Option<u64> {
    if value.is_nan() {
        None
    } else if value == 0.0 {
        Some(0)
    } else {
        Some(value.to_bits())
    }
}

fn float_as_exact_integer(value: f64) -> Option<i64> {
    if !value.is_finite() || value.fract() != 0.0 {
        return None;
    }
    const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
    const I64_LOWER_INCLUSIVE: f64 = -9_223_372_036_854_775_808.0;
    if !(I64_LOWER_INCLUSIVE..I64_UPPER_EXCLUSIVE).contains(&value) {
        return None;
    }
    Some(value as i64)
}
