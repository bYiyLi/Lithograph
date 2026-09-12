use crate::cypher::{Value, cypher_equals};

use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::require_exact_arity as require_arity;

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let result = match name {
        "coalesce" => coalesce(values),
        "nullif" => null_if(values),
        "property_exists" => property_exists(values),
        "elementid" => element_id(values),
        "id" => id(values),
        "labels" => labels(values),
        "type" => relationship_type(values),
        "length" | "path_length" => path_length(values),
        "nodes" => path_nodes(values),
        "relationships" => path_relationships(values),
        "properties" => properties(values),
        "keys" => keys(values),
        "db.namefromelementid" => database_name(values),
        _ => return None,
    };
    Some(result)
}

fn coalesce(values: &[Value]) -> QueryResult<Value> {
    if values.is_empty() {
        return Err(QueryError::semantic(
            "coalesce() requires at least one argument",
        ));
    }
    Ok(values
        .iter()
        .find(|value| !matches!(value, Value::Null))
        .cloned()
        .unwrap_or(Value::Null))
}

fn null_if(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2)?;
    Ok(if cypher_equals(&values[0], &values[1])? == Some(true) {
        Value::Null
    } else {
        values[0].clone()
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

fn element_id(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    element_id_value(&values[0])
}

pub(super) fn element_id_value(value: &Value) -> QueryResult<Value> {
    match value {
        Value::Node(node) => Ok(Value::String(node.element_id.clone())),
        Value::Relationship(relationship) => Ok(Value::String(relationship.element_id.clone())),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "elementId() requires Node or Relationship",
        )),
    }
}

fn id(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match element_id(values)? {
        Value::String(value) => value
            .split_once(':')
            .and_then(|(_, value)| value.parse::<i64>().ok())
            .map(Value::Integer)
            .ok_or_else(|| QueryError::semantic("element id has no numeric local component")),
        value => Ok(value),
    }
}

fn labels(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match &values[0] {
        Value::Node(node) => Ok(Value::List(
            node.labels.iter().cloned().map(Value::String).collect(),
        )),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "labels() requires Node",
        )),
    }
}

fn relationship_type(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match &values[0] {
        Value::Relationship(relationship) => {
            Ok(Value::String(relationship.relationship_type.clone()))
        }
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "type() requires Relationship",
        )),
    }
}

fn path_length(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match &values[0] {
        Value::Path(path) => Ok(Value::Integer(path.relationships.len() as i64)),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "length() requires Path",
        )),
    }
}

fn path_nodes(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match &values[0] {
        Value::Path(path) => Ok(Value::List(
            path.nodes.iter().cloned().map(Value::Node).collect(),
        )),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "nodes() requires Path",
        )),
    }
}

fn path_relationships(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match &values[0] {
        Value::Path(path) => Ok(Value::List(
            path.relationships
                .iter()
                .cloned()
                .map(Value::Relationship)
                .collect(),
        )),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "relationships() requires Path",
        )),
    }
}

fn properties(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match &values[0] {
        Value::Map(values) => Ok(Value::Map(values.clone())),
        Value::Node(node) => Ok(Value::Map(node.properties.clone())),
        Value::Relationship(relationship) => Ok(Value::Map(relationship.properties.clone())),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "properties() requires Map, Node, or Relationship",
        )),
    }
}

fn keys(values: &[Value]) -> QueryResult<Value> {
    let properties = properties(values)?;
    match properties {
        Value::Map(values) => Ok(Value::List(values.into_keys().map(Value::String).collect())),
        value => Ok(value),
    }
}

fn database_name(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match &values[0] {
        Value::String(value) if value.starts_with("n:") || value.starts_with("r:") => {
            Ok(Value::String("main".to_owned()))
        }
        Value::Null => Ok(Value::Null),
        Value::String(_) => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "db.nameFromElementId() requires String",
        )),
    }
}
