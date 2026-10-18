use crate::cypher::{self, Value};
use crate::storage::{IndexDefinition, PropertyValue};

use super::QueryResult;

/// Version state equality follows frozen LCE1 identity, not Cypher numeric
/// equality: all NaN payloads denote one state, while signed zeros differ.
pub(super) fn same_value(left: &Value, right: &Value) -> bool {
    match (left, right) {
        (Value::Float(left), Value::Float(right)) => {
            (left.is_nan() && right.is_nan()) || left.to_bits() == right.to_bits()
        }
        (Value::List(left), Value::List(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|(left, right)| same_value(left, right))
        }
        (Value::Map(left), Value::Map(right)) => {
            left.len() == right.len()
                && left
                    .iter()
                    .zip(right)
                    .all(|((key, left), (other, right))| key == other && same_value(left, right))
        }
        // Typed JSON retains coordinate type and signed-zero bits. Comparing
        // JsonValue directly would use numeric equality for its float Numbers.
        (Value::Point(_), Value::Point(_)) | (Value::Vector(_), Value::Vector(_)) => {
            cypher::encode_json_text(left) == cypher::encode_json_text(right)
        }
        _ => left == right,
    }
}

pub(super) fn same_optional_value(left: Option<&Value>, right: Option<&Value>) -> bool {
    match (left, right) {
        (Some(left), Some(right)) => same_value(left, right),
        (None, None) => true,
        _ => false,
    }
}

pub(super) fn same_property_value(
    left: Option<&PropertyValue>,
    right: Option<&PropertyValue>,
) -> QueryResult<bool> {
    Ok(left.map(PropertyValue::canonical_bytes).transpose()?
        == right.map(PropertyValue::canonical_bytes).transpose()?)
}

pub(super) fn same_index_definition(
    left: Option<&IndexDefinition>,
    right: Option<&IndexDefinition>,
) -> QueryResult<bool> {
    match (left, right) {
        (Some(left), Some(right)) => Ok(left.canonical_eq(right)?),
        (None, None) => Ok(true),
        _ => Ok(false),
    }
}
