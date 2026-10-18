use super::super::expression::{
    clone_map_controlled, clone_node_unreserved_controlled,
    clone_relationship_unreserved_controlled, clone_string_controlled,
    clone_value_unreserved_controlled, cypher_equals_controlled, value_bytes_with_control,
};
use super::super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, map_entry_bytes,
};
use crate::cypher::{NodeValue, RelationshipValue, Value};

use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{build_value, require_exact_arity as require_arity};

super::legacy_evaluators!();

pub(super) fn evaluate_with_memory(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    match name {
        "coalesce" => Some(coalesce(values, control, result_memory)),
        "nullif" => Some(null_if(values, control, result_memory)),
        "property_exists" => Some(property_exists(values)),
        _ => evaluate_unary(name, values, control, result_memory),
    }
}

fn evaluate_unary(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    const NAMES: &[&str] = &[
        "elementid",
        "id",
        "labels",
        "type",
        "length",
        "path_length",
        "nodes",
        "relationships",
        "properties",
        "keys",
        "db.namefromelementid",
    ];
    if !NAMES.contains(&name) {
        return None;
    }
    Some((|| {
        require_arity(values, 1)?;
        let value = &values[0];
        match name {
            "elementid" => match element_id_text(value)? {
                Some(text) => string_output(text, control, result_memory),
                None => Ok(Value::Null),
            },
            "id" => id(value),
            "labels" => labels(value, control, result_memory),
            "type" => relationship_type(value, control, result_memory),
            "length" | "path_length" => path_length(value),
            "nodes" => path_elements(value, true, control, result_memory),
            "relationships" => path_elements(value, false, control, result_memory),
            "properties" => property_projection(value, false, control, result_memory),
            "keys" => property_projection(value, true, control, result_memory),
            "db.namefromelementid" => database_name(value, control, result_memory),
            _ => Err(QueryError::internal("unknown structural projection")),
        }
    })())
}

fn coalesce(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    if values.is_empty() {
        return Err(QueryError::semantic(
            "coalesce() requires at least one argument",
        ));
    }
    for value in values {
        if let Some(control) = control {
            control.work(1)?;
        }
        if !matches!(value, Value::Null) {
            return clone_output(value, control, result_memory);
        }
    }
    Ok(Value::Null)
}

fn null_if(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 2)?;
    let equal = cypher_equals_controlled(&values[0], &values[1], control)?;
    Ok(if equal == Some(true) {
        Value::Null
    } else {
        clone_output(&values[0], control, result_memory)?
    })
}

fn property_exists(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2)?;
    if matches!(values[0], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::String(key) = &values[1] else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "property_exists() property name must be String",
        ));
    };
    let value = match &values[0] {
        Value::Node(node) => node.properties.get(key),
        Value::Relationship(relationship) => relationship.properties.get(key),
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "PROPERTY_EXISTS() requires Node or Relationship",
            ));
        }
    };
    Ok(Value::Boolean(
        value.is_some_and(|value| !matches!(value, Value::Null)),
    ))
}

fn element_id_text(value: &Value) -> QueryResult<Option<&str>> {
    match value {
        Value::Node(node) => Ok(Some(&node.element_id)),
        Value::Relationship(relationship) => Ok(Some(&relationship.element_id)),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "elementId() requires Node or Relationship",
        )),
    }
}

fn id(value: &Value) -> QueryResult<Value> {
    let Some(text) = element_id_text(value)? else {
        return Ok(Value::Null);
    };
    text.split_once(':')
        .and_then(|(_, value)| value.parse::<i64>().ok())
        .map(Value::Integer)
        .ok_or_else(|| QueryError::semantic("element id has no numeric local component"))
}

fn clone_output(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    build_value(
        value_bytes_with_control(value, control)?,
        control,
        result_memory,
        || clone_value_unreserved_controlled(value, control),
    )
}

fn string_output(
    text: &str,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    build_value(text.len() as u64, control, result_memory, || {
        clone_string_controlled(text, control).map(Value::String)
    })
}

fn project_list<T>(
    source: impl Clone + ExactSizeIterator<Item = T>,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
    size: impl Fn(T) -> QueryResult<u64>,
    construct: impl Fn(T) -> QueryResult<Value>,
) -> QueryResult<Value> {
    let bytes =
        source
            .clone()
            .try_fold(allocation_bytes::<Value>(source.len())?, |bytes, value| {
                if let Some(control) = control {
                    control.work(1)?;
                }
                checked_add(bytes, size(value)?)
            })?;
    build_value(bytes, control, result_memory, || {
        let mut output = Vec::with_capacity(source.len());
        for value in source {
            if let Some(control) = control {
                control.work(1)?;
            }
            output.push(construct(value)?);
        }
        Ok(Value::List(output))
    })
}

fn labels(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    match value {
        Value::Node(node) => project_list(
            node.labels.iter(),
            control,
            result_memory,
            |label| Ok(label.len() as u64),
            |label| clone_string_controlled(label, control).map(Value::String),
        ),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "labels() requires Node",
        )),
    }
}

fn relationship_type(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    match value {
        Value::Relationship(relationship) => {
            string_output(&relationship.relationship_type, control, result_memory)
        }
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "type() requires Relationship",
        )),
    }
}

fn path_length(value: &Value) -> QueryResult<Value> {
    match value {
        Value::Path(path) => Ok(Value::Integer(path.relationships.len() as i64)),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "length() requires Path",
        )),
    }
}

fn path_elements(
    value: &Value,
    nodes: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let path = match value {
        Value::Path(path) => path,
        Value::Null => return Ok(Value::Null),
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                if nodes {
                    "nodes() requires Path"
                } else {
                    "relationships() requires Path"
                },
            ));
        }
    };
    if nodes {
        project_list(
            path.nodes.iter(),
            control,
            result_memory,
            |node| node_projection_bytes(node, control),
            |node| clone_node_unreserved_controlled(node, control).map(Value::Node),
        )
    } else {
        project_list(
            path.relationships.iter(),
            control,
            result_memory,
            |relationship| relationship_projection_bytes(relationship, control),
            |relationship| {
                clone_relationship_unreserved_controlled(relationship, control)
                    .map(Value::Relationship)
            },
        )
    }
}

fn node_projection_bytes(
    node: &NodeValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    let bytes = checked_add(
        node.element_id.len() as u64,
        allocation_bytes::<String>(node.labels.len())?,
    )?;
    let bytes = node.labels.iter().try_fold(bytes, |bytes, label| {
        if let Some(control) = control {
            control.work(1)?;
        }
        checked_add(bytes, label.len() as u64)
    })?;
    checked_add(bytes, property_bytes(&node.properties, control)?)
}

fn relationship_projection_bytes(
    relationship: &RelationshipValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    let mut bytes = 0;
    for text in [
        &relationship.element_id,
        &relationship.relationship_type,
        &relationship.start,
        &relationship.end,
    ] {
        bytes = checked_add(bytes, text.len() as u64)?;
    }
    checked_add(bytes, property_bytes(&relationship.properties, control)?)
}

fn property_projection(
    value: &Value,
    keys: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let Some(properties) = property_map(value)? else {
        return Ok(Value::Null);
    };
    if keys {
        project_list(
            properties.keys(),
            control,
            result_memory,
            |key| Ok(key.len() as u64),
            |key| clone_string_controlled(key, control).map(Value::String),
        )
    } else {
        build_value(
            property_bytes(properties, control)?,
            control,
            result_memory,
            || clone_map_controlled(properties, control).map(Value::Map),
        )
    }
}

fn property_bytes(
    properties: &std::collections::BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    properties.iter().try_fold(0, |bytes, (key, value)| {
        if let Some(control) = control {
            control.work(1)?;
        }
        let bytes = checked_add(bytes, map_entry_bytes::<Value>()?)?;
        let bytes = checked_add(bytes, key.len() as u64)?;
        checked_add(bytes, value_bytes_with_control(value, control)?)
    })
}

fn property_map(value: &Value) -> QueryResult<Option<&std::collections::BTreeMap<String, Value>>> {
    match value {
        Value::Map(values) => Ok(Some(values)),
        Value::Node(node) => Ok(Some(&node.properties)),
        Value::Relationship(relationship) => Ok(Some(&relationship.properties)),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "properties() requires Map, Node, or Relationship",
        )),
    }
}

fn database_name(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    match value {
        Value::String(value) if value.starts_with("n:") || value.starts_with("r:") => {
            string_output("main", control, result_memory)
        }
        Value::Null | Value::String(_) => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "db.nameFromElementId() requires String",
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::resources::QueryResources;

    #[test]
    fn returned_keys_keep_output_lease() {
        let resources = QueryResources::new(Some(4096));
        let control = ExecutionControl::new(&resources, &|| false);
        let input = [Value::Map([("key".to_owned(), Value::Integer(1))].into())];
        let mut memory = None;
        let output = evaluate_with_memory("keys", &input, Some(&control), &mut memory)
            .unwrap()
            .unwrap();
        assert!(resources.used() > 0);
        drop(output);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn keys_does_not_copy_property_payload() {
        let resources = QueryResources::new(Some(4096));
        let control = ExecutionControl::new(&resources, &|| false);
        let input =
            Value::Map([("text".to_owned(), Value::String("x".repeat(1024 * 1024)))].into());
        let output = evaluate_controlled("keys", &[input], Some(&control))
            .unwrap()
            .unwrap();
        assert_eq!(output, Value::List(vec![Value::String("text".to_owned())]));
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn properties_reserves_before_copy() {
        let resources = QueryResources::new(Some(4096));
        let control = ExecutionControl::new(&resources, &|| false);
        let input =
            Value::Map([("text".to_owned(), Value::String("x".repeat(1024 * 1024)))].into());
        let error = evaluate_controlled("properties", &[input], Some(&control))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.peak(), 0);
        assert_eq!(resources.used(), 0);
    }
}
