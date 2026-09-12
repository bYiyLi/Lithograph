use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::cypher::{Value, cypher_equals, cypher_order_compare};

use super::super::spill::distinct_row_key;
use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{contains_null, require_arity};

const MAX_RANGE_VALUES: usize = 1_000_000;

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let result = match name {
        "cardinality" => cardinality(values),
        "size" => size(values),
        "isempty" => is_empty(values),
        "head" => endpoint(values, true),
        "last" => endpoint(values, false),
        "tail" => tail(values),
        "reverse" => reverse(values),
        "range" => range(values),
        "coll.distinct" => distinct(values),
        "coll.flatten" => flatten(values),
        "coll.indexof" => index_of(values),
        "coll.insert" => insert(values),
        "coll.max" => extreme(values, true),
        "coll.min" => extreme(values, false),
        "coll.remove" => remove(values),
        "coll.sort" => sort(values),
        _ => return None,
    };
    Some(result)
}

fn cardinality(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::List(value) => Ok(Value::Integer(value.len() as i64)),
        Value::Map(value) => Ok(Value::Integer(value.len() as i64)),
        Value::Path(value) => Ok(Value::Integer(
            value.nodes.len().saturating_add(value.relationships.len()) as i64,
        )),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "cardinality() requires Map, List, or Path input",
        )),
    }
}

fn size(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::List(value) => Ok(Value::Integer(value.len() as i64)),
        Value::String(value) => Ok(Value::Integer(value.chars().count() as i64)),
        Value::Vector(value) => Ok(Value::Integer(value.dimension() as i64)),
        Value::Null => Ok(Value::Null),
        _ => list_type_error(),
    }
}

fn is_empty(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::List(value) => Ok(Value::Boolean(value.is_empty())),
        Value::Map(value) => Ok(Value::Boolean(value.is_empty())),
        Value::String(value) => Ok(Value::Boolean(value.is_empty())),
        Value::Null => Ok(Value::Null),
        _ => list_type_error(),
    }
}

fn endpoint(values: &[Value], first: bool) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::List(values) => Ok(if first { values.first() } else { values.last() }
            .cloned()
            .unwrap_or(Value::Null)),
        Value::Null => Ok(Value::Null),
        _ => list_type_error(),
    }
}

fn tail(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::List(values) => Ok(Value::List(values.get(1..).unwrap_or_default().to_vec())),
        Value::Null => Ok(Value::Null),
        _ => list_type_error(),
    }
}

fn reverse(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::List(values) => Ok(Value::List(values.iter().rev().cloned().collect())),
        Value::String(value) => Ok(Value::String(value.chars().rev().collect())),
        Value::Null => Ok(Value::Null),
        _ => list_type_error(),
    }
}

fn range(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2, 3)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let start = integer(&values[0])?;
    let end = integer(&values[1])?;
    let step = values.get(2).map_or(Ok(1), integer)?;
    if step == 0 {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "range() step cannot be zero",
        ));
    }
    let mut output = Vec::new();
    let mut current = start;
    while (step > 0 && current <= end) || (step < 0 && current >= end) {
        if output.len() == MAX_RANGE_VALUES {
            return Err(QueryError::new(
                QueryErrorKind::Resource,
                "range() exceeds the query value limit",
            ));
        }
        output.push(Value::Integer(current));
        let Some(next) = current.checked_add(step) else {
            break;
        };
        current = next;
    }
    Ok(Value::List(output))
}

fn distinct(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    let Some(list) = nullable_list(&values[0])? else {
        return Ok(Value::Null);
    };
    let mut seen = BTreeSet::new();
    let mut output = Vec::new();
    for value in list {
        if seen.insert(distinct_row_key(std::slice::from_ref(value))?) {
            output.push(value.clone());
        }
    }
    Ok(Value::List(output))
}

fn flatten(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 2)?;
    let Some(list) = first_list_unless_null(values)? else {
        return Ok(Value::Null);
    };
    let depth = values.get(1).map_or(Ok(1_usize), |value| match value {
        Value::Integer(value) => usize::try_from(*value).map_err(|_| {
            QueryError::new(
                QueryErrorKind::Type,
                "coll.flatten() depth must be a non-negative Integer",
            )
        }),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "coll.flatten() depth must be a non-negative Integer",
        )),
    })?;
    let mut output = Vec::new();
    for value in list {
        flatten_value(value, depth, &mut output);
    }
    Ok(Value::List(output))
}

fn flatten_value(value: &Value, depth: usize, output: &mut Vec<Value>) {
    if depth > 0
        && let Value::List(values) = value
    {
        for value in values {
            flatten_value(value, depth - 1, output);
        }
    } else {
        output.push(value.clone());
    }
}

fn index_of(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    let Some(list) = first_list_unless_null(values)? else {
        return Ok(Value::Null);
    };
    for (index, value) in list.iter().enumerate() {
        if cypher_equals(value, &values[1])? == Some(true) {
            return Ok(Value::Integer(index as i64));
        }
    }
    Ok(Value::Integer(-1))
}

fn insert(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 3, 3)?;
    let Some(list) = first_list_unless_null(&values[..2])? else {
        return Ok(Value::Null);
    };
    let mut output = list.to_vec();
    let index = collection_index(integer(&values[1])?, output.len(), true)?;
    output.insert(index, values[2].clone());
    Ok(Value::List(output))
}

fn remove(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    let Some(list) = first_list_unless_null(values)? else {
        return Ok(Value::Null);
    };
    let mut output = list.to_vec();
    let index = collection_index(integer(&values[1])?, output.len(), false)?;
    output.remove(index);
    Ok(Value::List(output))
}

fn sort(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    let Some(list) = nullable_list(&values[0])? else {
        return Ok(Value::Null);
    };
    let mut output = list.to_vec();
    let mut failure = None;
    output.sort_by(|left, right| match cypher_order_compare(left, right) {
        Ok(ordering) => ordering,
        Err(error) => {
            failure = Some(error);
            Ordering::Equal
        }
    });
    if let Some(error) = failure {
        return Err(error.into());
    }
    Ok(Value::List(output))
}

fn extreme(values: &[Value], maximum: bool) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    let Some(list) = nullable_list(&values[0])? else {
        return Ok(Value::Null);
    };
    let mut candidate = None;
    for value in list.iter().filter(|value| !matches!(value, Value::Null)) {
        let replace = match candidate.as_ref() {
            None => true,
            Some(current) => {
                let ordering = cypher_order_compare(value, current)?;
                (maximum && ordering == Ordering::Greater)
                    || (!maximum && ordering == Ordering::Less)
            }
        };
        if replace {
            candidate = Some(value.clone());
        }
    }
    Ok(candidate.unwrap_or(Value::Null))
}

fn collection_index(index: i64, length: usize, allow_end: bool) -> QueryResult<usize> {
    let index = usize::try_from(index).map_err(|_| {
        QueryError::new(
            QueryErrorKind::Type,
            "collection index must be a non-negative Integer",
        )
    })?;
    if index < length || (allow_end && index == length) {
        Ok(index)
    } else {
        Err(QueryError::new(
            QueryErrorKind::Type,
            "collection index is outside the list",
        ))
    }
}

fn integer(value: &Value) -> QueryResult<i64> {
    match value {
        Value::Integer(value) => Ok(*value),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "List index/range bound must be Integer",
        )),
    }
}

fn list_value(value: &Value) -> QueryResult<&[Value]> {
    match value {
        Value::List(values) => Ok(values),
        _ => list_type_error(),
    }
}

fn nullable_list(value: &Value) -> QueryResult<Option<&[Value]>> {
    match value {
        Value::Null => Ok(None),
        _ => list_value(value).map(Some),
    }
}

fn first_list_unless_null(values: &[Value]) -> QueryResult<Option<&[Value]>> {
    if contains_null(values) {
        Ok(None)
    } else {
        list_value(&values[0]).map(Some)
    }
}

fn list_type_error<T>() -> QueryResult<T> {
    Err(QueryError::new(
        QueryErrorKind::Type,
        "List function requires List input",
    ))
}
