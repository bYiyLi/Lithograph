use std::cmp::Ordering;
use std::collections::BTreeSet;

use crate::cypher::Value;

use super::super::expression::{
    clone_value_unreserved_controlled, cypher_equals_controlled, cypher_order_compare_controlled,
    value_bytes_with_control,
};
use super::super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, checked_mul,
};
use super::super::spill::distinct_row_key;
use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{build_value, contains_null, require_arity};

const MAX_RANGE_VALUES: usize = 2_000_000;

super::legacy_evaluators!();

pub(super) fn evaluate_with_memory(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    let result = match name {
        "cardinality" => cardinality(values),
        "size" => size(values, control),
        "isempty" => is_empty(values),
        "head" => endpoint(values, true, control, result_memory),
        "last" => endpoint(values, false, control, result_memory),
        "tail" => tail(values, control, result_memory),
        "reverse" => reverse(values, control, result_memory),
        "range" => range(values, control, result_memory),
        "coll.distinct" => distinct(values, control, result_memory),
        "coll.flatten" => flatten(values, control, result_memory),
        "coll.indexof" => index_of(values, control),
        "coll.insert" => insert(values, control, result_memory),
        "coll.max" => extreme(values, true, control, result_memory),
        "coll.min" => extreme(values, false, control, result_memory),
        "coll.remove" => remove(values, control, result_memory),
        "coll.sort" => sort(values, control, result_memory),
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

fn size(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::List(value) => Ok(Value::Integer(value.len() as i64)),
        Value::String(value) => {
            let mut length = 0_i64;
            for character in value.chars() {
                work(control, 1)?;
                if let Some(control) = control {
                    control.bytes(character.len_utf8() as u64)?;
                }
                length += 1;
            }
            Ok(Value::Integer(length))
        }
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

fn endpoint(
    values: &[Value],
    first: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    with_unary_list(values, |values| {
        copy_optional(
            if first { values.first() } else { values.last() },
            control,
            result_memory,
        )
    })
}

fn tail(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    with_unary_list(values, |values| {
        copy_list(
            values.get(1..).unwrap_or_default(),
            false,
            control,
            result_memory,
        )
    })
}

fn reverse(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::List(values) => copy_list(values, true, control, result_memory),
        Value::String(value) => build_value(value.len() as u64, control, result_memory, || {
            let mut output = String::with_capacity(value.len());
            for character in value.chars().rev() {
                work(control, 1)?;
                if let Some(control) = control {
                    control.bytes(character.len_utf8() as u64)?;
                }
                output.push(character);
            }
            Ok(Value::String(output))
        }),
        Value::Null => Ok(Value::Null),
        _ => list_type_error(),
    }
}

fn copy_list(
    values: &[Value],
    reverse: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    copy_selected(
        values.len(),
        ordered_values(values, reverse),
        control,
        result_memory,
    )
}

fn copy_selected<'a>(
    length: usize,
    values: impl IntoIterator<Item = &'a Value>,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let (output, memory) = clone_selected(length, values, control)?;
    *result_memory = memory;
    Ok(Value::List(output))
}

fn clone_list(
    values: &[Value],
    reverse: bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(Vec<Value>, Option<MemoryReservation>)> {
    clone_selected(values.len(), ordered_values(values, reverse), control)
}

fn ordered_values(values: &[Value], reverse: bool) -> impl Iterator<Item = &Value> {
    (0..values.len()).map(move |index| {
        &values[if reverse {
            values.len() - index - 1
        } else {
            index
        }]
    })
}

fn clone_selected<'a>(
    length: usize,
    values: impl IntoIterator<Item = &'a Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(Vec<Value>, Option<MemoryReservation>)> {
    let mut memory = reserve(control, allocation_bytes::<Value>(length)?)?;
    let mut output = Vec::with_capacity(length);
    for value in values {
        work(control, 1)?;
        grow(&mut memory, heap_bytes(value, control)?)?;
        output.push(clone_value_unreserved_controlled(value, control)?);
    }
    Ok((output, memory))
}

fn range(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
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
    // i128 arithmetic also handles i64::MIN steps and opposite endpoint extremes.
    let distance = (i128::from(end) - i128::from(start)) * i128::from(step.signum());
    let count = if distance < 0 {
        0
    } else {
        distance / i128::from(step).abs() + 1
    };
    if count > MAX_RANGE_VALUES as i128 {
        return Err(QueryError::new(
            QueryErrorKind::Resource,
            "range() exceeds the query value limit",
        ));
    }
    let count = count as usize;
    build_value(
        allocation_bytes::<Value>(count)?,
        control,
        result_memory,
        || {
            let mut output = Vec::with_capacity(count);
            let mut current = i128::from(start);
            for _ in 0..count {
                work(control, 1)?;
                output.push(Value::Integer(current as i64));
                current += i128::from(step);
            }
            Ok(Value::List(output))
        },
    )
}

fn distinct(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    with_unary_list(values, |list| {
        let mut memory = reserve(control, 0)?;
        let mut seen_memory = reserve(control, 0)?;
        let mut seen = BTreeSet::new();
        let mut output = Vec::new();
        for value in list {
            work(control, 1)?;
            let (key, _key_memory) = collection_key(value, control)?;
            if !seen.contains(&key) {
                grow(&mut seen_memory, checked_add(key.capacity() as u64, 128)?)?;
                seen.insert(key);
                append_copy(&mut output, value, &mut memory, control)?;
            }
        }
        *result_memory = memory;
        Ok(Value::List(output))
    })
}

fn collection_key(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(String, Option<MemoryReservation>)> {
    // The key encoder constructs tagged JSON and its escaped serialization.
    // Hold a conservative temporary allowance before either is allocated.
    let temporary = checked_add(
        checked_mul(value_bytes_with_control(value, control)?, 8)?,
        256,
    )?;
    let memory = reserve(control, temporary)?;
    let key = distinct_row_key(std::slice::from_ref(value), control)?;
    Ok((key, memory))
}

fn flatten(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
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
    let mut memory = reserve(control, 0)?;
    let mut output = Vec::new();
    for value in list {
        flatten_value(value, depth, &mut output, &mut memory, control)?;
    }
    *result_memory = memory;
    Ok(Value::List(output))
}

fn flatten_value(
    value: &Value,
    depth: usize,
    output: &mut Vec<Value>,
    memory: &mut Option<MemoryReservation>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    work(control, 1)?;
    if depth > 0
        && let Value::List(values) = value
    {
        for value in values {
            flatten_value(value, depth - 1, output, memory, control)?;
        }
    } else {
        append_copy(output, value, memory, control)?;
    }
    Ok(())
}

fn index_of(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    let Some(list) = first_list_unless_null(values)? else {
        return Ok(Value::Null);
    };
    for (index, value) in list.iter().enumerate() {
        work(control, 1)?;
        if cypher_equals_controlled(value, &values[1], control)? == Some(true) {
            return Ok(Value::Integer(index as i64));
        }
    }
    Ok(Value::Integer(-1))
}

fn insert(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 3, 3)?;
    let Some(list) = first_list_unless_null(&values[..2])? else {
        return Ok(Value::Null);
    };
    let index = collection_index(integer(&values[1])?, list.len(), true)?;
    let length = list.len().checked_add(1).ok_or_else(capacity_error)?;
    let selected = list[..index]
        .iter()
        .chain(std::iter::once(&values[2]))
        .chain(&list[index..]);
    copy_selected(length, selected, control, result_memory)
}

fn remove(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    let Some(list) = first_list_unless_null(values)? else {
        return Ok(Value::Null);
    };
    let index = collection_index(integer(&values[1])?, list.len(), false)?;
    let length = list.len() - 1;
    copy_selected(
        length,
        list[..index].iter().chain(&list[index + 1..]),
        control,
        result_memory,
    )
}

fn sort(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    with_unary_list(values, |list| {
        let (mut output, memory) = clone_list(list, false, control)?;
        // Stable slice sort may allocate scratch storage; reserve its full upper bound.
        let _scratch = reserve(control, allocation_bytes::<Value>(output.len())?)?;
        let mut failure = None;
        output.sort_by(|left, right| {
            if failure.is_some() {
                return Ordering::Equal;
            }
            match work(control, 1)
                .and_then(|()| cypher_order_compare_controlled(left, right, control))
            {
                Ok(ordering) => ordering,
                Err(error) => {
                    failure = Some(error);
                    Ordering::Equal
                }
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }
        if let Some(control) = control {
            control.check_now()?;
        }
        *result_memory = memory;
        Ok(Value::List(output))
    })
}

fn extreme(
    values: &[Value],
    maximum: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    with_unary_list(values, |list| {
        // Borrow the candidate during the scan: only the final result is copied.
        let mut candidate = None;
        for value in list {
            work(control, 1)?;
            if matches!(value, Value::Null) {
                continue;
            }
            let replace = match candidate {
                None => true,
                Some(current) => farther(value, current, maximum, control)?,
            };
            if replace {
                candidate = Some(value);
            }
        }
        copy_optional(candidate, control, result_memory)
    })
}

fn copy_optional(
    value: Option<&Value>,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    match value {
        Some(value) => build_value(
            value_bytes_with_control(value, control)?,
            control,
            result_memory,
            || clone_value_unreserved_controlled(value, control),
        ),
        None => Ok(Value::Null),
    }
}

fn farther(
    value: &Value,
    current: &Value,
    maximum: bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    let ordering = cypher_order_compare_controlled(value, current, control)?;
    Ok(if maximum {
        ordering == Ordering::Greater
    } else {
        ordering == Ordering::Less
    })
}

fn append_copy(
    output: &mut Vec<Value>,
    value: &Value,
    memory: &mut Option<MemoryReservation>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    if output.len() == output.capacity() {
        let capacity = output
            .capacity()
            .max(1)
            .checked_mul(2)
            .ok_or_else(capacity_error)?;
        let additional = capacity - output.capacity();
        grow(memory, allocation_bytes::<Value>(additional)?)?;
        output.reserve_exact(capacity - output.len());
    }
    grow(memory, heap_bytes(value, control)?)?;
    output.push(clone_value_unreserved_controlled(value, control)?);
    Ok(())
}

fn heap_bytes(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<u64> {
    Ok(value_bytes_with_control(value, control)? - std::mem::size_of::<Value>() as u64)
}

fn reserve(
    control: Option<&ExecutionControl<'_>>,
    bytes: u64,
) -> QueryResult<Option<MemoryReservation>> {
    control.map(|control| control.reserve(bytes)).transpose()
}

fn grow(memory: &mut Option<MemoryReservation>, bytes: u64) -> QueryResult<()> {
    if let Some(memory) = memory {
        memory.grow(bytes)?;
    }
    Ok(())
}

fn work(control: Option<&ExecutionControl<'_>>, units: u64) -> QueryResult<()> {
    control.map_or(Ok(()), |control| control.work(units))
}

fn capacity_error() -> QueryError {
    QueryError::new(
        QueryErrorKind::Resource,
        "collection capacity exceeds the query value limit",
    )
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

fn with_unary_list(
    values: &[Value],
    operation: impl FnOnce(&[Value]) -> QueryResult<Value>,
) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match nullable_list(&values[0])? {
        Some(values) => operation(values),
        None => Ok(Value::Null),
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

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::query::resources::QueryResources;

    fn integers(values: &[i64]) -> Vec<Value> {
        values.iter().copied().map(Value::Integer).collect()
    }

    #[test]
    fn range_counts_extreme_endpoints_without_overflow() {
        for (arguments, expected) in [
            (integers(&[i64::MAX, i64::MAX]), integers(&[i64::MAX])),
            (integers(&[i64::MIN, i64::MIN, -1]), integers(&[i64::MIN])),
            (
                integers(&[i64::MAX, i64::MIN, i64::MIN]),
                integers(&[i64::MAX, -1]),
            ),
            (integers(&[3, 1]), vec![]),
            (integers(&[1, 3, -1]), vec![]),
            (integers(&[4, -1, -2]), integers(&[4, 2, 0])),
        ] {
            assert_eq!(
                range(&arguments, None, &mut None).unwrap(),
                Value::List(expected)
            );
        }
        assert_eq!(
            range(&integers(&[i64::MIN, i64::MAX]), None, &mut None)
                .unwrap_err()
                .kind,
            QueryErrorKind::Resource
        );
        assert_eq!(
            range(&integers(&[1, 5, 0]), None, &mut None)
                .unwrap_err()
                .kind,
            QueryErrorKind::Type
        );
    }

    #[test]
    fn range_reserves_full_output_before_allocating() {
        let arguments = integers(&[1, 500_000]);
        let small = QueryResources::new(Some(16 * 1024 * 1024));
        let never = || false;
        let control = ExecutionControl::new(&small, &never);
        assert_eq!(
            range(&arguments, Some(&control), &mut None)
                .unwrap_err()
                .kind,
            QueryErrorKind::Resource
        );
        assert_eq!(small.peak(), 0);
        assert_eq!(small.used(), 0);

        let large = QueryResources::new(Some(256 * 1024 * 1024));
        let control = ExecutionControl::new(&large, &never);
        let Value::List(output) = range(&arguments, Some(&control), &mut None).unwrap() else {
            panic!("range did not return a List");
        };
        assert_eq!(output.len(), 500_000);
        assert_eq!(output.first(), Some(&Value::Integer(1)));
        assert_eq!(output.last(), Some(&Value::Integer(500_000)));
        assert_eq!(output.capacity(), output.len());
        assert_eq!(large.used(), 0);
    }

    #[test]
    fn collection_loops_observe_cancellation_and_release_memory() {
        for function in [
            "range",
            "reverse",
            "tail",
            "coll.flatten",
            "coll.distinct",
            "coll.max",
        ] {
            let checks = Cell::new(0_u32);
            let interrupt = || {
                checks.set(checks.get() + 1);
                checks.get() >= 4
            };
            let resources = QueryResources::new(Some(16 * 1024 * 1024));
            let control = ExecutionControl::new(&resources, &interrupt);
            let values = if function == "range" {
                integers(&[1, 10_000])
            } else {
                vec![Value::List((1..10_000).map(Value::Integer).collect())]
            };
            let error = evaluate_controlled(function, &values, Some(&control))
                .expect("known function")
                .unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted, "{function}");
            assert_eq!(resources.used(), 0, "{function}");
        }
    }

    #[test]
    fn copied_and_flattened_values_reserve_heap_before_copying() {
        let values = vec![Value::List(vec![Value::String("x".repeat(4096))])];
        let resources = QueryResources::new(Some(2048));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        for function in ["head", "tail", "reverse", "coll.flatten", "coll.sort"] {
            // tail of a single-element list is empty; exercise a retained string.
            let input = if function == "tail" {
                vec![Value::List(vec![
                    Value::Null,
                    Value::String("x".repeat(4096)),
                ])]
            } else {
                values.clone()
            };
            assert_eq!(
                evaluate_controlled(function, &input, Some(&control))
                    .unwrap()
                    .unwrap_err()
                    .kind,
                QueryErrorKind::Resource,
                "{function}"
            );
            assert_eq!(resources.used(), 0, "{function}");
        }
    }

    #[test]
    fn stable_collection_sort_preserves_equal_value_representations() {
        let values = vec![Value::List(vec![
            Value::Float(2.0),
            Value::Integer(1),
            Value::Float(1.0),
            Value::Integer(2),
        ])];
        assert_eq!(
            evaluate("coll.sort", &values).unwrap().unwrap(),
            Value::List(vec![
                Value::Integer(1),
                Value::Float(1.0),
                Value::Float(2.0),
                Value::Integer(2),
            ])
        );
    }

    #[test]
    fn returned_range_keeps_its_exact_budget_claim_until_consumer_drop() {
        let bytes = allocation_bytes::<Value>(256).unwrap();
        let resources = QueryResources::new(Some(bytes));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let mut result_memory = None;
        let output = evaluate_with_memory(
            "range",
            &integers(&[1, 256]),
            Some(&control),
            &mut result_memory,
        )
        .unwrap()
        .unwrap();
        assert_eq!(resources.used(), bytes);
        assert_eq!(result_memory.as_ref().unwrap().bytes(), bytes);
        assert_eq!(
            resources.reserve(1).unwrap_err().kind,
            QueryErrorKind::Resource
        );
        drop(output);
        drop(result_memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn collection_allocation_paths_transfer_their_output_claims() {
        let resources = QueryResources::new(Some(1024 * 1024));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let list = Value::List(vec![
            Value::String("x".repeat(512)),
            Value::String("y".repeat(512)),
        ]);
        for (name, arguments) in [
            ("head", vec![list.clone()]),
            ("last", vec![list.clone()]),
            ("tail", vec![list.clone()]),
            ("reverse", vec![list.clone()]),
            ("reverse", vec![Value::String("人😀".repeat(512))]),
            ("range", integers(&[1, 32])),
            ("coll.distinct", vec![list.clone()]),
            ("coll.flatten", vec![list.clone()]),
            (
                "coll.insert",
                vec![list.clone(), Value::Integer(1), Value::Integer(4)],
            ),
            ("coll.max", vec![list.clone()]),
            ("coll.min", vec![list.clone()]),
            ("coll.remove", vec![list.clone(), Value::Integer(1)]),
            ("coll.sort", vec![list]),
        ] {
            let mut result_memory = None;
            let output = evaluate_with_memory(name, &arguments, Some(&control), &mut result_memory)
                .unwrap()
                .unwrap();
            let lease = result_memory.expect("allocated collection output retains its claim");
            assert!(lease.bytes() > 0, "function: {name}");
            assert_eq!(resources.used(), lease.bytes(), "function: {name}");
            drop(output);
            drop(lease);
            assert_eq!(resources.used(), 0, "function: {name}");
        }
    }
}
