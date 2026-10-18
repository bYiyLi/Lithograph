//! Cypher comparison loops with execution cancellation checkpoints.

use std::cmp::Ordering;
use std::collections::BTreeMap;

use crate::cypher::{
    self, CypherComparison, PathValue, Value, VectorValue, VectorValues, sort_float,
    vector_type_rank,
};
use crate::query::resources::ExecutionControl;
use crate::query::{QueryError, QueryErrorKind, QueryResult};

pub(crate) fn cypher_equals_controlled(
    left: &Value,
    right: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<bool>> {
    work(control)?;
    match (left, right) {
        (Value::String(left), Value::String(right)) => Ok(Some(
            left.len() == right.len() && bytes_compare(left, right, control)? == Ordering::Equal,
        )),
        (Value::List(left), Value::List(right)) => list_equals(left, right, control),
        (Value::Map(left), Value::Map(right)) => map_equals(left, right, control),
        (Value::Vector(left), Value::Vector(right)) => vector_equals(left, right, control),
        (Value::Node(left), Value::Node(right)) => Ok(Some(
            bytes_compare(&left.element_id, &right.element_id, control)? == Ordering::Equal,
        )),
        (Value::Relationship(left), Value::Relationship(right)) => Ok(Some(
            bytes_compare(&left.element_id, &right.element_id, control)? == Ordering::Equal,
        )),
        (Value::Path(left), Value::Path(right)) => path_equals(left, right, control).map(Some),
        (Value::Path(path), Value::List(values)) | (Value::List(values), Value::Path(path)) => {
            path_list_equals(path, values, control).map(Some)
        }
        _ => cypher::cypher_equals(left, right).map_err(Into::into),
    }
}

fn vector_equals(
    left: &VectorValue,
    right: &VectorValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<bool>> {
    if left.coordinate_type() != right.coordinate_type() || left.dimension() != right.dimension() {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "VECTOR values with different coordinate types or dimensions are not equality-comparable",
        ));
    }
    let equal = match (left.values(), right.values()) {
        (VectorValues::I8(left), VectorValues::I8(right)) => {
            coordinates_equal(left, right, control)?
        }
        (VectorValues::I16(left), VectorValues::I16(right)) => {
            coordinates_equal(left, right, control)?
        }
        (VectorValues::I32(left), VectorValues::I32(right)) => {
            coordinates_equal(left, right, control)?
        }
        (VectorValues::I64(left), VectorValues::I64(right)) => {
            coordinates_equal(left, right, control)?
        }
        (VectorValues::F32(left), VectorValues::F32(right)) => {
            coordinates_equal(left, right, control)?
        }
        (VectorValues::F64(left), VectorValues::F64(right)) => {
            coordinates_equal(left, right, control)?
        }
        _ => false,
    };
    Ok(Some(equal))
}

fn coordinates_equal<T: PartialEq>(
    left: &[T],
    right: &[T],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    for (left, right) in left.iter().zip(right) {
        work(control)?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

fn list_equals(
    left: &[Value],
    right: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<bool>> {
    if left.len() != right.len() {
        return Ok(Some(false));
    }
    pairs_equal(left.iter().zip(right), control)
}

fn map_equals(
    left: &BTreeMap<String, Value>,
    right: &BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<bool>> {
    if left.len() != right.len() || map_keys(left, right, control)? != Ordering::Equal {
        return Ok(Some(false));
    }
    pairs_equal(left.values().zip(right.values()), control)
}

fn pairs_equal<'value>(
    pairs: impl IntoIterator<Item = (&'value Value, &'value Value)>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<bool>> {
    let mut unknown = false;
    for (left, right) in pairs {
        match cypher_equals_controlled(left, right, control)? {
            Some(false) => return Ok(Some(false)),
            Some(true) => {}
            None => unknown = true,
        }
    }
    Ok((!unknown).then_some(true))
}

fn path_equals(
    left: &PathValue,
    right: &PathValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    if left.nodes.len() != right.nodes.len()
        || left.relationships.len() != right.relationships.len()
    {
        return Ok(false);
    }
    // Equality checks all node identities, then all relationship identities.
    for (left, right) in left
        .nodes
        .iter()
        .map(|node| &node.element_id)
        .chain(
            left.relationships
                .iter()
                .map(|relationship| &relationship.element_id),
        )
        .zip(
            right.nodes.iter().map(|node| &node.element_id).chain(
                right
                    .relationships
                    .iter()
                    .map(|relationship| &relationship.element_id),
            ),
        )
    {
        work(control)?;
        if bytes_compare(left, right, control)? != Ordering::Equal {
            return Ok(false);
        }
    }
    Ok(true)
}

fn path_list_equals(
    path: &PathValue,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    if path.nodes.len() != path.relationships.len().saturating_add(1)
        || values.len() != path.nodes.len().saturating_add(path.relationships.len())
    {
        return Ok(false);
    }
    for (index, value) in values.iter().enumerate() {
        work(control)?;
        let identity = match (index % 2, value) {
            (0, Value::Node(node)) => &node.element_id,
            (1, Value::Relationship(relationship)) => &relationship.element_id,
            _ => return Ok(false),
        };
        if bytes_compare(path_identity(path, index), identity, control)? != Ordering::Equal {
            return Ok(false);
        }
    }
    Ok(true)
}

pub(crate) fn cypher_compare_controlled(
    left: &Value,
    right: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<CypherComparison>> {
    work(control)?;
    match (left, right) {
        (Value::String(left), Value::String(right)) => {
            Ok(Some(bytes_compare(left, right, control)?.into()))
        }
        (Value::List(left), Value::List(right)) => {
            for (left, right) in left.iter().zip(right) {
                work(control)?;
                if matches!(left, Value::Null) || matches!(right, Value::Null) {
                    return Ok(None);
                }
                let ordering = cypher_order_compare_controlled(left, right, control)?;
                if ordering != Ordering::Equal {
                    return Ok(Some(ordering.into()));
                }
            }
            Ok(Some(left.len().cmp(&right.len()).into()))
        }
        (Value::Map(_), Value::Map(_))
        | (Value::Path(_), Value::Path(_))
        | (Value::Node(_), Value::Node(_))
        | (Value::Relationship(_), Value::Relationship(_)) => {
            cypher_order_compare_controlled(left, right, control).map(|value| Some(value.into()))
        }
        _ => cypher::cypher_compare(left, right).map_err(Into::into),
    }
}

pub(crate) fn cypher_order_compare_controlled(
    left: &Value,
    right: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    work(control)?;
    match (left, right) {
        (Value::String(left), Value::String(right)) => bytes_compare(left, right, control),
        (Value::Node(left), Value::Node(right)) => {
            bytes_compare(&left.element_id, &right.element_id, control)
        }
        (Value::Relationship(left), Value::Relationship(right)) => {
            bytes_compare(&left.element_id, &right.element_id, control)
        }
        (Value::List(left), Value::List(right)) => list_order(left, right, control),
        (Value::Map(left), Value::Map(right)) => map_order(left, right, control),
        (Value::Path(left), Value::Path(right)) => path_order(left, right, control),
        (Value::Vector(left), Value::Vector(right)) => vector_order(left, right, control),
        _ => cypher::cypher_order_compare(left, right).map_err(Into::into),
    }
}

fn vector_order(
    left: &VectorValue,
    right: &VectorValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    let coordinate_type =
        vector_type_rank(left.coordinate_type()).cmp(&vector_type_rank(right.coordinate_type()));
    if coordinate_type != Ordering::Equal {
        return Ok(coordinate_type);
    }
    let dimension = left.dimension().cmp(&right.dimension());
    if dimension != Ordering::Equal {
        return Ok(dimension);
    }
    vector_coordinate_order(left.values(), right.values(), control)
}

fn vector_coordinate_order(
    left: &VectorValues,
    right: &VectorValues,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    match (left, right) {
        (VectorValues::I8(left), VectorValues::I8(right)) => {
            coordinates_order(left, right, i8::cmp, control)
        }
        (VectorValues::I16(left), VectorValues::I16(right)) => {
            coordinates_order(left, right, i16::cmp, control)
        }
        (VectorValues::I32(left), VectorValues::I32(right)) => {
            coordinates_order(left, right, i32::cmp, control)
        }
        (VectorValues::I64(left), VectorValues::I64(right)) => {
            coordinates_order(left, right, i64::cmp, control)
        }
        (VectorValues::F32(left), VectorValues::F32(right)) => coordinates_order(
            left,
            right,
            |left, right| sort_float(f64::from(*left), f64::from(*right)),
            control,
        ),
        (VectorValues::F64(left), VectorValues::F64(right)) => coordinates_order(
            left,
            right,
            |left, right| sort_float(*left, *right),
            control,
        ),
        _ => unreachable!("matching vector type ranks require matching coordinate storage"),
    }
}

fn coordinates_order<T>(
    left: &[T],
    right: &[T],
    mut compare: impl FnMut(&T, &T) -> Ordering,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    for (left, right) in left.iter().zip(right) {
        work(control)?;
        let order = compare(left, right);
        if order != Ordering::Equal {
            return Ok(order);
        }
    }
    Ok(Ordering::Equal)
}

fn list_order(
    left: &[Value],
    right: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    for (left, right) in left.iter().zip(right) {
        let ordering = cypher_order_compare_controlled(left, right, control)?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

fn map_order(
    left: &BTreeMap<String, Value>,
    right: &BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    let size = left.len().cmp(&right.len());
    if size != Ordering::Equal {
        return Ok(size);
    }
    let keys = map_keys(left, right, control)?;
    if keys != Ordering::Equal {
        return Ok(keys);
    }
    for (left, right) in left.values().zip(right.values()) {
        let ordering = cypher_order_compare_controlled(left, right, control)?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(Ordering::Equal)
}

fn map_keys(
    left: &BTreeMap<String, Value>,
    right: &BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    for (left, right) in left.keys().zip(right.keys()) {
        work(control)?;
        let ordering = bytes_compare(left, right, control)?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

fn path_order(
    left: &PathValue,
    right: &PathValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    if left.nodes.len() != left.relationships.len().saturating_add(1)
        || right.nodes.len() != right.relationships.len().saturating_add(1)
    {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "PATH requires exactly one more node than relationship",
        ));
    }
    let left_len = left.nodes.len().saturating_add(left.relationships.len());
    let right_len = right.nodes.len().saturating_add(right.relationships.len());
    for index in 0..left_len.min(right_len) {
        work(control)?;
        let ordering = bytes_compare(
            path_identity(left, index),
            path_identity(right, index),
            control,
        )?;
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(left_len.cmp(&right_len))
}

fn path_identity(path: &PathValue, index: usize) -> &str {
    if index.is_multiple_of(2) {
        &path.nodes[index / 2].element_id
    } else {
        &path.relationships[index / 2].element_id
    }
}

pub(super) fn bytes_compare(
    left: &str,
    right: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Ordering> {
    for (left, right) in left
        .as_bytes()
        .chunks(64 * 1024)
        .zip(right.as_bytes().chunks(64 * 1024))
    {
        if let Some(control) = control {
            control.bytes(left.len().min(right.len()) as u64)?;
        }
        let ordering = left.cmp(right);
        if ordering != Ordering::Equal {
            return Ok(ordering);
        }
    }
    Ok(left.len().cmp(&right.len()))
}

fn work(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    control.map_or(Ok(()), |control| control.work(1))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::cypher::VectorCoordinateType;
    use crate::query::resources::QueryResources;

    fn vectors() -> Vec<Value> {
        [
            (VectorCoordinateType::I8, VectorValues::I8(vec![1, 2])),
            (VectorCoordinateType::I8, VectorValues::I8(vec![1])),
            (VectorCoordinateType::I16, VectorValues::I16(vec![1, 2])),
            (VectorCoordinateType::I32, VectorValues::I32(vec![1, 2])),
            (
                VectorCoordinateType::I64,
                VectorValues::I64(vec![i64::MAX - 1]),
            ),
            (VectorCoordinateType::I64, VectorValues::I64(vec![i64::MAX])),
            (
                VectorCoordinateType::F32,
                VectorValues::F32(vec![-0.0, 1.25]),
            ),
            (
                VectorCoordinateType::F32,
                VectorValues::F32(vec![0.0, 1.25]),
            ),
            (
                VectorCoordinateType::F64,
                VectorValues::F64(vec![-0.0, 1.25]),
            ),
            (
                VectorCoordinateType::F64,
                VectorValues::F64(vec![0.0, 1.25]),
            ),
        ]
        .into_iter()
        .map(|(kind, values)| Value::Vector(VectorValue::new(kind, values).unwrap()))
        .collect()
    }

    #[test]
    fn controlled_vectors_preserve_type_dimension_coordinate_and_zero_semantics() {
        let resources = QueryResources::default();
        let never = || false;
        let control = Some(ExecutionControl::new(&resources, &never));
        let vectors = vectors();
        for left in &vectors {
            for right in &vectors {
                assert_comparisons_match_cypher(left, right, control.as_ref());
                assert_eq!(
                    cypher_compare_controlled(left, right, control.as_ref()).unwrap(),
                    None
                );
            }
        }
    }

    #[test]
    fn float_coordinates_keep_nan_and_signed_zero_comparison_rules() {
        assert!(!coordinates_equal(&[f64::NAN], &[f64::NAN], None).unwrap());
        assert!(!coordinates_equal(&[f32::NAN], &[f32::NAN], None).unwrap());
        assert!(coordinates_equal(&[-0.0_f64], &[0.0_f64], None).unwrap());
        assert_eq!(sort_float(f64::NAN, f64::NAN), Ordering::Equal);
        assert_eq!(sort_float(f64::NAN, 1.0), Ordering::Greater);
        assert_eq!(sort_float(1.0, f64::NAN), Ordering::Less);
        assert_eq!(sort_float(-0.0, 0.0), Ordering::Equal);
    }

    #[test]
    fn vector_equality_and_ordering_poll_inside_coordinate_traversal() {
        for (kind, values) in [
            (VectorCoordinateType::I8, VectorValues::I8(vec![1; 4_096])),
            (VectorCoordinateType::I16, VectorValues::I16(vec![1; 4_096])),
            (VectorCoordinateType::I32, VectorValues::I32(vec![1; 4_096])),
            (VectorCoordinateType::I64, VectorValues::I64(vec![1; 4_096])),
            (
                VectorCoordinateType::F32,
                VectorValues::F32(vec![1.0; 4_096]),
            ),
            (
                VectorCoordinateType::F64,
                VectorValues::F64(vec![1.0; 4_096]),
            ),
        ] {
            let value = Value::Vector(VectorValue::new(kind, values).unwrap());
            for equality in [true, false] {
                assert_two_checks_interrupt(|control| {
                    if equality {
                        cypher_equals_controlled(&value, &value, Some(control)).map(|_| ())
                    } else {
                        cypher_order_compare_controlled(&value, &value, Some(control)).map(|_| ())
                    }
                });
            }
        }
    }

    #[test]
    fn controlled_comparisons_preserve_cypher_null_type_and_order_semantics() {
        let mut values = vec![
            Value::Null,
            Value::Boolean(false),
            Value::Integer(1),
            Value::Float(1.0),
            Value::Float(f64::NAN),
            Value::String("人物".into()),
            Value::List(vec![Value::Null]),
            Value::List(vec![Value::Integer(1)]),
            Value::List(vec![Value::List(vec![Value::Null])]),
            Value::Map(BTreeMap::from([("a".into(), Value::Null)])),
            Value::Map(BTreeMap::from([("a".into(), Value::Integer(1))])),
            Value::Map(BTreeMap::from([("b".into(), Value::String("x".into()))])),
        ];
        let node = crate::cypher::NodeValue {
            element_id: "node1".into(),
            labels: vec!["人物".into()],
            properties: BTreeMap::new(),
        };
        let other = crate::cypher::NodeValue {
            element_id: "node2".into(),
            ..node.clone()
        };
        let relationship = crate::cypher::RelationshipValue {
            element_id: "rel1".into(),
            relationship_type: "认识".into(),
            start: "node1".into(),
            end: "node2".into(),
            properties: BTreeMap::new(),
        };
        values.extend([
            Value::Node(node.clone()),
            Value::Relationship(relationship.clone()),
            Value::Path(PathValue {
                nodes: vec![node.clone(), other.clone()],
                relationships: vec![relationship.clone()],
            }),
            Value::List(vec![
                Value::Node(node.clone()),
                Value::Relationship(relationship.clone()),
                Value::Node(other),
            ]),
            Value::Path(PathValue {
                nodes: vec![node],
                relationships: vec![relationship],
            }),
        ]);
        let resources = QueryResources::default();
        let never = || false;
        let control = Some(ExecutionControl::new(&resources, &never));
        for left in &values {
            for right in &values {
                assert_comparisons_match_cypher(left, right, control.as_ref());
            }
        }
    }

    #[test]
    fn nested_list_and_map_comparisons_poll_inside_a_single_value() {
        let list = Value::List((0..20_000).map(Value::Integer).collect());
        let map = Value::Map(BTreeMap::from([("nested".into(), list.clone())]));
        let wide = Value::Map(
            (0..20_000)
                .map(|index| (format!("key{index:05}"), Value::Null))
                .collect(),
        );
        for value in [list, map, wide] {
            for comparison in 0..3 {
                assert_two_checks_interrupt(|control| match comparison {
                    0 => cypher_equals_controlled(&value, &value, Some(control)).map(|_| ()),
                    1 => cypher_compare_controlled(&value, &value, Some(control)).map(|_| ()),
                    _ => cypher_order_compare_controlled(&value, &value, Some(control)).map(|_| ()),
                });
            }
        }
    }

    #[test]
    fn string_comparison_polls_at_sixty_four_kibibyte_boundaries() {
        let value = Value::String("人".repeat(100_000));
        assert_two_checks_interrupt(|control| {
            cypher_equals_controlled(&value, &value, Some(control)).map(|_| ())
        });
    }

    fn assert_two_checks_interrupt(compare: impl FnOnce(&ExecutionControl<'_>) -> QueryResult<()>) {
        let checks = Cell::new(0);
        let interrupt = || {
            checks.set(checks.get() + 1);
            checks.get() >= 2
        };
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &interrupt);
        assert_eq!(
            compare(&control).unwrap_err().kind,
            QueryErrorKind::Interrupted
        );
        assert_eq!(checks.get(), 2);
    }

    fn assert_comparisons_match_cypher(
        left: &Value,
        right: &Value,
        control: Option<&ExecutionControl<'_>>,
    ) {
        assert_eq!(
            cypher_equals_controlled(left, right, control),
            cypher::cypher_equals(left, right).map_err(QueryError::from),
            "equality {left:?} {right:?}"
        );
        assert_eq!(
            cypher_compare_controlled(left, right, control),
            cypher::cypher_compare(left, right).map_err(QueryError::from),
            "comparison {left:?} {right:?}"
        );
        assert_eq!(
            cypher_order_compare_controlled(left, right, control),
            cypher::cypher_order_compare(left, right).map_err(QueryError::from),
            "ordering {left:?} {right:?}"
        );
    }
}
