use std::cmp::Ordering;
use std::collections::BTreeMap;

use super::temporal::{compare_time, compare_zoned_datetime};
use super::value::{
    PathValue, PointValue, Value, ValueError, VectorCoordinateType, VectorValue, VectorValues,
    integer_float_cmp,
};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_DAY: i128 = 86_400 * NANOS_PER_SECOND;
const DURATION_DENOMINATOR: i128 = 1_600;
const AVERAGE_MONTH_DAYS_NUMERATOR: i128 = 48_699;

/// Total value ordering used by `ORDER BY` for the Phase 03 value families.
///
/// This is deliberately separate from [`super::value::cypher_compare`], which implements the
/// `<`, `<=`, `>`, and `>=` operators and therefore rejects values such as Point and Vector.
/// The frozen 2026.08 public evidence introduces UUID but does not define its value ordering
/// semantics, so UUID ordering remains owned by the later compatibility phase.
pub fn cypher_order_compare(left: &Value, right: &Value) -> Result<Ordering, ValueError> {
    match (left, right) {
        (Value::Uuid(_), _) | (_, Value::Uuid(_)) => {
            return Err(ValueError::new(
                "UUID ORDER BY semantics are not defined by the frozen public Cypher 25 evidence",
            ));
        }
        _ => {}
    }

    let left_rank = value_order_rank(left);
    let right_rank = value_order_rank(right);
    if left_rank != right_rank {
        return Ok(left_rank.cmp(&right_rank));
    }

    match (left, right) {
        (Value::Map(left), Value::Map(right)) => map_order_compare(left, right),
        (Value::Node(left), Value::Node(right)) => Ok(left.element_id.cmp(&right.element_id)),
        (Value::Relationship(left), Value::Relationship(right)) => {
            Ok(left.element_id.cmp(&right.element_id))
        }
        (Value::List(left), Value::List(right)) => sequence_order_compare(left, right),
        (Value::Path(left), Value::Path(right)) => Ok(path_order_compare(left, right)),
        (Value::Vector(left), Value::Vector(right)) => Ok(vector_order_compare(left, right)),
        (Value::Point(left), Value::Point(right)) => Ok(point_order_compare(left, right)),
        (Value::ZonedDateTime(left), Value::ZonedDateTime(right)) => {
            Ok(compare_zoned_datetime(left, right))
        }
        (Value::LocalDateTime(left), Value::LocalDateTime(right)) => {
            Ok(left.comparison_key().cmp(&right.comparison_key()))
        }
        (Value::Date(left), Value::Date(right)) => Ok(left.days().cmp(&right.days())),
        (Value::Time(left), Value::Time(right)) => Ok(compare_time(left, right)),
        (Value::LocalTime(left), Value::LocalTime(right)) => {
            Ok(left.nanoseconds().cmp(&right.nanoseconds()))
        }
        (Value::Duration(left), Value::Duration(right)) => {
            Ok(duration_sort_key(left.components()).cmp(&duration_sort_key(right.components())))
        }
        (Value::String(left), Value::String(right)) => Ok(left.cmp(right)),
        (Value::Boolean(left), Value::Boolean(right)) => Ok(left.cmp(right)),
        (Value::Integer(left), Value::Integer(right)) => Ok(left.cmp(right)),
        (Value::Integer(left), Value::Float(right)) => Ok(sort_integer_float(*left, *right)),
        (Value::Float(left), Value::Integer(right)) => {
            Ok(sort_integer_float(*right, *left).reverse())
        }
        (Value::Float(left), Value::Float(right)) => Ok(sort_float(*left, *right)),
        (Value::Null, Value::Null) => Ok(Ordering::Equal),
        _ => Err(ValueError::new(
            "internal value ordering rank collision between different Cypher value families",
        )),
    }
}

fn value_order_rank(value: &Value) -> u8 {
    match value {
        Value::Map(_) => 0,
        Value::Node(_) => 1,
        Value::Relationship(_) => 2,
        Value::List(_) => 3,
        Value::Path(_) => 4,
        Value::Vector(_) => 5,
        Value::Point(_) => 6,
        Value::ZonedDateTime(_) => 7,
        Value::LocalDateTime(_) => 8,
        Value::Date(_) => 9,
        Value::Time(_) => 10,
        Value::LocalTime(_) => 11,
        Value::Duration(_) => 12,
        Value::String(_) => 13,
        Value::Boolean(_) => 14,
        Value::Integer(_) | Value::Float(_) => 15,
        Value::Null => 16,
        Value::Uuid(_) => unreachable!("UUID is handled before value ranking"),
    }
}

fn sequence_order_compare(left: &[Value], right: &[Value]) -> Result<Ordering, ValueError> {
    for (left, right) in left.iter().zip(right) {
        let ordering = cypher_order_compare(left, right)?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

fn map_order_compare(
    left: &BTreeMap<String, Value>,
    right: &BTreeMap<String, Value>,
) -> Result<Ordering, ValueError> {
    let size = left.len().cmp(&right.len());
    if size != Ordering::Equal {
        return Ok(size);
    }
    let keys = left.keys().cmp(right.keys());
    if keys != Ordering::Equal {
        return Ok(keys);
    }
    for key in left.keys() {
        let ordering = cypher_order_compare(&left[key], &right[key])?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

fn path_order_compare(left: &PathValue, right: &PathValue) -> Ordering {
    let left_len = left.nodes.len().saturating_add(left.relationships.len());
    let right_len = right.nodes.len().saturating_add(right.relationships.len());
    for index in 0..left_len.min(right_len) {
        let ordering = if index % 2 == 0 {
            left.nodes[index / 2]
                .element_id
                .cmp(&right.nodes[index / 2].element_id)
        } else {
            left.relationships[index / 2]
                .element_id
                .cmp(&right.relationships[index / 2].element_id)
        };
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    left_len.cmp(&right_len)
}

fn point_order_compare(left: &PointValue, right: &PointValue) -> Ordering {
    let srid = left.srid().cmp(&right.srid());
    if srid != Ordering::Equal {
        return srid;
    }
    for (left, right) in left.coordinates().iter().zip(right.coordinates()) {
        let ordering = left.partial_cmp(right).unwrap_or(Ordering::Equal);
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn vector_order_compare(left: &VectorValue, right: &VectorValue) -> Ordering {
    let coordinate_type =
        vector_type_rank(left.coordinate_type()).cmp(&vector_type_rank(right.coordinate_type()));
    if coordinate_type != Ordering::Equal {
        return coordinate_type;
    }
    let dimension = left.dimension().cmp(&right.dimension());
    if dimension != Ordering::Equal {
        return dimension;
    }
    match (left.values(), right.values()) {
        (VectorValues::I8(left), VectorValues::I8(right)) => left.cmp(right),
        (VectorValues::I16(left), VectorValues::I16(right)) => left.cmp(right),
        (VectorValues::I32(left), VectorValues::I32(right)) => left.cmp(right),
        (VectorValues::I64(left), VectorValues::I64(right)) => left.cmp(right),
        (VectorValues::F32(left), VectorValues::F32(right)) => float_sequence_compare(left, right),
        (VectorValues::F64(left), VectorValues::F64(right)) => float_sequence_compare(left, right),
        _ => unreachable!("coordinate type rank equality guarantees matching vector storage"),
    }
}

fn vector_type_rank(value: VectorCoordinateType) -> u8 {
    match value {
        VectorCoordinateType::I8 => 0,
        VectorCoordinateType::I16 => 1,
        VectorCoordinateType::I32 => 2,
        VectorCoordinateType::I64 => 3,
        VectorCoordinateType::F32 => 4,
        VectorCoordinateType::F64 => 5,
    }
}

fn float_sequence_compare<T: Copy + Into<f64>>(left: &[T], right: &[T]) -> Ordering {
    for (left, right) in left.iter().copied().zip(right.iter().copied()) {
        let ordering = sort_float(left.into(), right.into());
        if ordering != Ordering::Equal {
            return ordering;
        }
    }
    Ordering::Equal
}

fn duration_sort_key((months, days, seconds, nanoseconds): (i64, i64, i64, i64)) -> i128 {
    i128::from(months) * AVERAGE_MONTH_DAYS_NUMERATOR * NANOS_PER_DAY
        + i128::from(days) * DURATION_DENOMINATOR * NANOS_PER_DAY
        + i128::from(seconds) * DURATION_DENOMINATOR * NANOS_PER_SECOND
        + i128::from(nanoseconds) * DURATION_DENOMINATOR
}

fn sort_integer_float(integer: i64, float: f64) -> Ordering {
    if float.is_nan() {
        Ordering::Less
    } else {
        integer_float_cmp(integer, float).unwrap_or(Ordering::Equal)
    }
}

fn sort_float(left: f64, right: f64) -> Ordering {
    match (left.is_nan(), right.is_nan()) {
        (true, true) => Ordering::Equal,
        (true, false) => Ordering::Greater,
        (false, true) => Ordering::Less,
        (false, false) => left.partial_cmp(&right).unwrap_or(Ordering::Equal),
    }
}
