use std::collections::BTreeMap;

use crate::cypher::{NodeValue, PathValue, RelationshipValue, Value, VectorValue, VectorValues};
use crate::query::QueryResult;
use crate::query::access::PropertyRules;
#[cfg(test)]
use crate::query::resources::MemoryReservation;
use crate::query::resources::{ExecutionControl, value_bytes, value_bytes_controlled};

#[cfg(test)]
pub(crate) fn reserve_value(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<MemoryReservation>> {
    control
        .map(|control| control.reserve(value_bytes_controlled(value, control)?))
        .transpose()
}

pub(crate) fn value_bytes_with_control(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    control.map_or_else(
        || value_bytes(value),
        |control| value_bytes_controlled(value, control),
    )
}

pub(crate) fn clone_string_controlled(
    value: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<String> {
    if let Some(control) = control {
        control.work(1)?;
    }
    let mut output = String::with_capacity(value.len());
    let mut offset = 0;
    while offset < value.len() {
        let mut end = (offset + 64 * 1024).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        if let Some(control) = control {
            control.bytes((end - offset) as u64)?;
        }
        output.push_str(&value[offset..end]);
        offset = end;
    }
    Ok(output)
}

pub(crate) fn clone_map_controlled(
    values: &BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<BTreeMap<String, Value>> {
    values
        .iter()
        .map(|(key, value)| {
            Ok((
                clone_string_controlled(key, control)?,
                clone_value_unreserved_controlled(value, control)?,
            ))
        })
        .collect()
}

pub(crate) fn clone_node_unreserved_controlled(
    node: &NodeValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<NodeValue> {
    clone_node_with_properties(node, control, |properties| {
        clone_map_controlled(properties, control)
    })
}

fn clone_node_with_properties(
    node: &NodeValue,
    control: Option<&ExecutionControl<'_>>,
    properties: impl FnOnce(&BTreeMap<String, Value>) -> QueryResult<BTreeMap<String, Value>>,
) -> QueryResult<NodeValue> {
    if let Some(control) = control {
        control.work(1)?;
    }
    Ok(NodeValue {
        element_id: clone_string_controlled(&node.element_id, control)?,
        labels: collect_exact(&node.labels, |label| {
            clone_string_controlled(label, control)
        })?,
        properties: properties(&node.properties)?,
    })
}

pub(crate) fn clone_relationship_unreserved_controlled(
    value: &RelationshipValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<RelationshipValue> {
    clone_relationship_with_properties(value, control, |properties| {
        clone_map_controlled(properties, control)
    })
}

fn clone_relationship_with_properties(
    value: &RelationshipValue,
    control: Option<&ExecutionControl<'_>>,
    properties: impl FnOnce(&BTreeMap<String, Value>) -> QueryResult<BTreeMap<String, Value>>,
) -> QueryResult<RelationshipValue> {
    if let Some(control) = control {
        control.work(1)?;
    }
    Ok(RelationshipValue {
        element_id: clone_string_controlled(&value.element_id, control)?,
        relationship_type: clone_string_controlled(&value.relationship_type, control)?,
        start: clone_string_controlled(&value.start, control)?,
        end: clone_string_controlled(&value.end, control)?,
        properties: properties(&value.properties)?,
    })
}

pub(crate) fn clone_value_unreserved_controlled(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    if let Some(control) = control {
        control.work(1)?;
    }
    Ok(match value {
        Value::String(value) => Value::String(clone_string_controlled(value, control)?),
        Value::List(values) => Value::List(collect_exact(values, |value| {
            clone_value_unreserved_controlled(value, control)
        })?),
        Value::Map(values) => Value::Map(clone_map_controlled(values, control)?),
        Value::Node(value) => Value::Node(clone_node_unreserved_controlled(value, control)?),
        Value::Relationship(value) => {
            Value::Relationship(clone_relationship_unreserved_controlled(value, control)?)
        }
        Value::Path(value) => Value::Path(PathValue {
            nodes: collect_exact(&value.nodes, |value| {
                clone_node_unreserved_controlled(value, control)
            })?,
            relationships: collect_exact(&value.relationships, |value| {
                clone_relationship_unreserved_controlled(value, control)
            })?,
        }),
        Value::Vector(value) => Value::Vector(clone_vector_controlled(value, control)?),
        value => value.clone(),
    })
}

fn clone_vector_controlled(
    value: &VectorValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<VectorValue> {
    let values = match value.values() {
        VectorValues::I8(values) => VectorValues::I8(clone_copy_slice_controlled(values, control)?),
        VectorValues::I16(values) => {
            VectorValues::I16(clone_copy_slice_controlled(values, control)?)
        }
        VectorValues::I32(values) => {
            VectorValues::I32(clone_copy_slice_controlled(values, control)?)
        }
        VectorValues::I64(values) => {
            VectorValues::I64(clone_copy_slice_controlled(values, control)?)
        }
        VectorValues::F32(values) => {
            VectorValues::F32(clone_copy_slice_controlled(values, control)?)
        }
        VectorValues::F64(values) => {
            VectorValues::F64(clone_copy_slice_controlled(values, control)?)
        }
    };
    VectorValue::new_with_checkpoint(value.coordinate_type(), values, |units| match control {
        Some(control) => control.work(units as u64),
        None => Ok(()),
    })
}

pub(super) fn clone_copy_slice_controlled<T: Copy>(
    values: &[T],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Vec<T>> {
    let mut output = Vec::with_capacity(values.len());
    for chunk in values.chunks(256) {
        if let Some(control) = control {
            control.work(chunk.len() as u64)?;
        }
        output.extend_from_slice(chunk);
    }
    Ok(output)
}

/// Copies an already-reserved value and skips denied graph properties before
/// copying their values. Ordinary Map keys are retained, and nested graph
/// objects inside ordinary Maps and Lists receive the same access policy.
pub(crate) fn clone_filtered_value_unreserved_controlled(
    value: &Value,
    access: &PropertyRules,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    if access.is_empty() {
        return clone_value_unreserved_controlled(value, control);
    }
    if let Some(control) = control {
        control.work(1)?;
    }
    Ok(match value {
        Value::List(values) => Value::List(collect_exact(values, |value| {
            clone_filtered_value_unreserved_controlled(value, access, control)
        })?),
        Value::Map(values) => Value::Map(clone_filtered_map(values, access, control)?),
        Value::Node(node) => Value::Node(clone_filtered_node(node, access, control)?),
        Value::Relationship(value) => {
            Value::Relationship(clone_filtered_relationship(value, access, control)?)
        }
        Value::Path(value) => Value::Path(PathValue {
            nodes: collect_exact(&value.nodes, |node| {
                clone_filtered_node(node, access, control)
            })?,
            relationships: collect_exact(&value.relationships, |relationship| {
                clone_filtered_relationship(relationship, access, control)
            })?,
        }),
        value => clone_value_unreserved_controlled(value, control)?,
    })
}

fn clone_filtered_map(
    values: &BTreeMap<String, Value>,
    access: &PropertyRules,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<BTreeMap<String, Value>> {
    values
        .iter()
        .map(|(key, value)| {
            Ok((
                clone_string_controlled(key, control)?,
                clone_filtered_value_unreserved_controlled(value, access, control)?,
            ))
        })
        .collect()
}

fn clone_filtered_node(
    value: &NodeValue,
    access: &PropertyRules,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<NodeValue> {
    clone_node_with_properties(value, control, |properties| {
        clone_filtered_properties(properties, access, control)
    })
}

fn clone_filtered_relationship(
    value: &RelationshipValue,
    access: &PropertyRules,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<RelationshipValue> {
    clone_relationship_with_properties(value, control, |properties| {
        clone_filtered_properties(properties, access, control)
    })
}

fn clone_filtered_properties(
    values: &BTreeMap<String, Value>,
    access: &PropertyRules,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<BTreeMap<String, Value>> {
    let mut output = BTreeMap::new();
    for (key, value) in values {
        if let Some(control) = control {
            control.work(1)?;
        }
        if access.denies(key) {
            continue;
        }
        output.insert(
            clone_string_controlled(key, control)?,
            clone_value_unreserved_controlled(value, control)?,
        );
    }
    Ok(output)
}

fn collect_exact<T, U>(
    values: &[T],
    mut evaluate: impl FnMut(&T) -> QueryResult<U>,
) -> QueryResult<Vec<U>> {
    let mut output = Vec::with_capacity(values.len());
    for value in values {
        output.push(evaluate(value)?);
    }
    Ok(output)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use serde_json::json;

    use crate::cypher::VectorCoordinateType;
    use crate::query::{ExecutionOptions, QueryErrorKind, QueryResources};

    use super::*;

    fn access() -> PropertyRules {
        ExecutionOptions::parse(&json!({
            "readAccess": {"query": {"denyPropertyPrefixes": ["secret"]}}
        }))
        .unwrap()
        .access
        .read
        .query
    }

    fn node() -> NodeValue {
        NodeValue {
            element_id: "n:1".to_owned(),
            labels: vec!["Person".to_owned()],
            properties: BTreeMap::from([
                ("public".to_owned(), Value::Integer(1)),
                ("secretValue".to_owned(), Value::String("hidden".to_owned())),
            ]),
        }
    }

    #[test]
    fn filtered_copy_preserves_ordinary_maps_and_filters_nested_graph_values() {
        let relationship = RelationshipValue {
            element_id: "r:1".to_owned(),
            relationship_type: "KNOWS".to_owned(),
            start: "n:1".to_owned(),
            end: "n:2".to_owned(),
            properties: node().properties,
        };
        let value = Value::List(vec![
            Value::Node(node()),
            Value::Relationship(relationship.clone()),
            Value::Path(PathValue {
                nodes: vec![node()],
                relationships: vec![relationship],
            }),
            Value::Map(BTreeMap::from([
                ("secretMapKey".to_owned(), Value::Integer(2)),
                ("nested".to_owned(), Value::Node(node())),
            ])),
        ]);
        let access = access();
        assert_eq!(
            clone_filtered_value_unreserved_controlled(&value, &access, None).unwrap(),
            access.filter_graph_properties(&value)
        );
    }

    #[test]
    fn denied_property_values_are_skipped_before_their_contents_are_copied() {
        let mut value = node();
        value.properties.insert(
            "secretLarge".to_owned(),
            Value::String("x".repeat(1024 * 1024)),
        );
        let value = Value::Node(value);
        let resources = QueryResources::default();
        let memory = resources.reserve(value_bytes(&value).unwrap()).unwrap();
        let interrupt = || true;
        let control = ExecutionControl::new(&resources, &interrupt);
        let result =
            clone_filtered_value_unreserved_controlled(&value, &access(), Some(&control)).unwrap();
        let Value::Node(result) = result else {
            panic!("Node expected");
        };
        assert_eq!(
            result.properties,
            BTreeMap::from([("public".to_owned(), Value::Integer(1))])
        );
        assert_eq!(resources.used(), memory.bytes());
    }

    #[test]
    fn filtering_many_denied_properties_polls_cancellation_inside_the_loop() {
        let mut value = node();
        value.properties = (0..10_000)
            .map(|index| (format!("secret{index:05}"), Value::Integer(index)))
            .collect();
        let value = Value::Node(value);
        let resources = QueryResources::default();
        let memory = resources.reserve(value_bytes(&value).unwrap()).unwrap();
        let checks = Cell::new(0);
        let interrupt = || {
            checks.set(checks.get() + 1);
            true
        };
        let control = ExecutionControl::new(&resources, &interrupt);
        let error = clone_filtered_value_unreserved_controlled(&value, &access(), Some(&control))
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(checks.get(), 1);
        assert_eq!(resources.used(), memory.bytes());
        drop(memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn filtered_list_and_label_copies_keep_the_reserved_exact_capacity() {
        let mut node = node();
        node.labels = (0..5).map(|index| format!("label{index}")).collect();
        node.labels.shrink_to_fit();
        let mut values = vec![Value::Node(node); 5];
        values.shrink_to_fit();
        let value = Value::List(values);
        let result = clone_filtered_value_unreserved_controlled(&value, &access(), None).unwrap();
        let Value::List(values) = &result else {
            panic!("List expected");
        };
        assert_eq!(values.capacity(), 5);
        for value in values {
            let Value::Node(node) = value else {
                panic!("Node expected");
            };
            assert_eq!(node.labels.capacity(), 5);
        }
        assert!(value_bytes(&result).unwrap() <= value_bytes(&value).unwrap());
    }

    #[test]
    fn vector_copy_cancels_within_256_final_finite_checks() {
        const DIMENSION: usize = 4096;
        const COPY_CHECKS: usize = DIMENSION / 256;
        for (coordinate_type, values) in [
            (
                VectorCoordinateType::F32,
                VectorValues::F32(vec![1.0; DIMENSION]),
            ),
            (
                VectorCoordinateType::F64,
                VectorValues::F64(vec![1.0; DIMENSION]),
            ),
        ] {
            let vector = VectorValue::new(coordinate_type, values).unwrap();
            let checks = Cell::new(0);
            let interrupted = || {
                checks.set(checks.get() + 1);
                checks.get() > COPY_CHECKS
            };
            let resources = QueryResources::default();
            let control = ExecutionControl::new(&resources, &interrupted);
            let error = clone_vector_controlled(&vector, Some(&control)).unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
            assert_eq!(checks.get(), COPY_CHECKS + 1);
            assert_eq!(resources.used(), 0);
        }
    }
}
