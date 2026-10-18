use crate::cypher::{self, CypherComparison, Value};
use crate::query::resources::{ExecutionControl, MemoryReservation, allocation_bytes, checked_add};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

use super::{BinaryOp, UnaryOp, value_bytes_with_control};

#[path = "compare_control.rs"]
mod compare_control;
pub(crate) use compare_control::{
    cypher_compare_controlled, cypher_equals_controlled, cypher_order_compare_controlled,
};

pub(super) fn evaluate_unary(op: UnaryOp, value: Value) -> QueryResult<Value> {
    match (op, value) {
        (UnaryOp::Not, Value::Boolean(value)) => Ok(Value::Boolean(!value)),
        (UnaryOp::Not, Value::Null) => Ok(Value::Null),
        (UnaryOp::Positive, value @ (Value::Integer(_) | Value::Float(_))) => Ok(value),
        (UnaryOp::Negative, Value::Integer(value)) => value
            .checked_neg()
            .map(Value::Integer)
            .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "INTEGER64 negation overflow")),
        (UnaryOp::Negative, Value::Float(value)) => Ok(Value::Float(-value)),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "unary operator received an incompatible value",
        )),
    }
}

pub(super) fn evaluate_binary(op: BinaryOp, left: Value, right: Value) -> QueryResult<Value> {
    match op {
        BinaryOp::Or | BinaryOp::Xor | BinaryOp::And => boolean_binary(op, left, right),
        BinaryOp::Equal | BinaryOp::NotEqual => {
            let value = cypher::cypher_equals(&left, &right)?;
            Ok(match value {
                Some(value) => Value::Boolean(if op == BinaryOp::NotEqual {
                    !value
                } else {
                    value
                }),
                None => Value::Null,
            })
        }
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            ordered_binary(op, &left, &right)
        }
        BinaryOp::In => in_operator(left, right, None),
        BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => {
            string_predicate(op, left, right)
        }
        BinaryOp::Regex => regex_predicate(left, right),
        BinaryOp::Add
        | BinaryOp::Concat
        | BinaryOp::Subtract
        | BinaryOp::Multiply
        | BinaryOp::Divide
        | BinaryOp::Modulo
        | BinaryOp::Power => numeric_binary(op, left, right),
    }
}

#[cfg(test)]
pub(super) fn evaluate_binary_controlled(
    op: BinaryOp,
    left: Value,
    right: Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    evaluate_binary_with_memory(op, left, right, control, &mut None)
}

pub(super) fn evaluate_binary_with_memory(
    op: BinaryOp,
    left: Value,
    right: Value,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let memory = control
        .map(|control| control.reserve(binary_destination_bytes(op, &left, &right, control)?))
        .transpose()?;
    let result = evaluate_binary_impl(op, left, right, control)?;
    *result_memory = memory;
    Ok(result)
}

fn binary_destination_bytes(
    op: BinaryOp,
    left: &Value,
    right: &Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let inline = std::mem::size_of::<Value>() as u64;
    if matches!(
        op,
        BinaryOp::Add | BinaryOp::Subtract | BinaryOp::Multiply | BinaryOp::Divide
    ) && (matches!(left, Value::Duration(_)) || matches!(right, Value::Duration(_)))
    {
        // Temporal arithmetic owns short canonical strings and bounded formatting
        // scratch, plus a possible copied timezone supplied by either operand.
        return checked_add(
            1024,
            checked_add(
                value_bytes_with_control(left, Some(control))?,
                value_bytes_with_control(right, Some(control))?,
            )?,
        );
    }
    if matches!(op, BinaryOp::Add | BinaryOp::Concat) {
        return match (left, right) {
            (Value::String(left), Value::String(right)) => {
                checked_add(inline, checked_add(left.len() as u64, right.len() as u64)?)
            }
            (Value::List(_), _) | (_, Value::List(_)) if op == BinaryOp::Add => checked_add(
                value_bytes_with_control(left, Some(control))?,
                value_bytes_with_control(right, Some(control))?,
            ),
            (Value::List(_), Value::List(_)) => checked_add(
                value_bytes_with_control(left, Some(control))?,
                value_bytes_with_control(right, Some(control))?,
            ),
            _ => Ok(inline),
        };
    }
    Ok(inline)
}

fn evaluate_binary_impl(
    op: BinaryOp,
    left: Value,
    right: Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    let Some(control) = control else {
        return evaluate_binary(op, left, right);
    };
    control.check()?;
    if op == BinaryOp::In {
        return in_operator(left, right, Some(control));
    }
    match op {
        BinaryOp::Equal | BinaryOp::NotEqual => {
            return equality_binary_controlled(op, &left, &right, control);
        }
        BinaryOp::Less | BinaryOp::LessEqual | BinaryOp::Greater | BinaryOp::GreaterEqual => {
            return ordered_binary_controlled(op, &left, &right, control);
        }
        BinaryOp::StartsWith | BinaryOp::EndsWith | BinaryOp::Contains => {
            return string_predicate_controlled(op, left, right, control);
        }
        BinaryOp::Regex => return regex_predicate_controlled(left, right, control),
        _ => {}
    }
    let result = match (op, left, right) {
        (BinaryOp::Add | BinaryOp::Concat, Value::String(left), Value::String(right)) => {
            let capacity = left.len().checked_add(right.len()).ok_or_else(|| {
                QueryError::new(QueryErrorKind::Resource, "String length overflow")
            })?;
            let mut output = String::with_capacity(capacity);
            for input in [&left, &right] {
                crate::query::functions::append_text_controlled(&mut output, input, Some(control))?;
            }
            Ok(Value::String(output))
        }
        (BinaryOp::Add | BinaryOp::Concat, Value::List(left), Value::List(right)) => {
            concatenate_lists(left, right, control)
        }
        (BinaryOp::Add, Value::List(left), right) => append_element(left, right, true, control),
        (BinaryOp::Add, left, Value::List(right)) => append_element(right, left, false, control),
        (op, left, right) => evaluate_binary(op, left, right),
    };
    control.check()?;
    result
}

fn concatenate_lists(
    left: Vec<Value>,
    right: Vec<Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    let capacity = left
        .len()
        .checked_add(right.len())
        .ok_or_else(|| QueryError::new(QueryErrorKind::Resource, "List length overflow"))?;
    let mut output = Vec::with_capacity(capacity);
    for value in left.into_iter().chain(right) {
        control.work(1)?;
        output.push(value);
    }
    Ok(Value::List(output))
}

fn append_element(
    values: Vec<Value>,
    element: Value,
    append: bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    let capacity = values
        .len()
        .checked_add(1)
        .ok_or_else(|| QueryError::new(QueryErrorKind::Resource, "List length overflow"))?;
    let mut output = Vec::with_capacity(capacity);
    if append {
        for value in values.into_iter().chain(std::iter::once(element)) {
            control.work(1)?;
            output.push(value);
        }
    } else {
        for value in std::iter::once(element).chain(values) {
            control.work(1)?;
            output.push(value);
        }
    }
    Ok(Value::List(output))
}

fn equality_binary_controlled(
    op: BinaryOp,
    left: &Value,
    right: &Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    Ok(
        match cypher_equals_controlled(left, right, Some(control))? {
            Some(value) => Value::Boolean(if op == BinaryOp::NotEqual {
                !value
            } else {
                value
            }),
            None => Value::Null,
        },
    )
}

fn ordered_binary_controlled(
    op: BinaryOp,
    left: &Value,
    right: &Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    Ok(
        match cypher_compare_controlled(left, right, Some(control))? {
            None => Value::Null,
            Some(comparison) => Value::Boolean(comparison_matches(op, comparison)),
        },
    )
}

fn comparison_matches(op: BinaryOp, comparison: CypherComparison) -> bool {
    match comparison {
        CypherComparison::Unordered => false,
        CypherComparison::Less => matches!(op, BinaryOp::Less | BinaryOp::LessEqual),
        CypherComparison::Equal => matches!(op, BinaryOp::LessEqual | BinaryOp::GreaterEqual),
        CypherComparison::Greater => matches!(op, BinaryOp::Greater | BinaryOp::GreaterEqual),
    }
}

fn string_predicate_controlled(
    op: BinaryOp,
    left: Value,
    right: Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    let (Value::String(left), Value::String(right)) = (left, right) else {
        return Ok(Value::Null);
    };
    let value = match op {
        BinaryOp::Contains => contains_controlled(left.as_bytes(), right.as_bytes(), control)?,
        BinaryOp::StartsWith => match left.as_bytes().get(..right.len()) {
            Some(prefix) => bytes_equal(prefix, right.as_bytes(), control)?,
            None => false,
        },
        BinaryOp::EndsWith => match left.len().checked_sub(right.len()) {
            Some(start) => bytes_equal(&left.as_bytes()[start..], right.as_bytes(), control)?,
            None => false,
        },
        _ => return Err(QueryError::internal("invalid String predicate operator")),
    };
    control.check()?;
    Ok(Value::Boolean(value))
}

fn bytes_equal(left: &[u8], right: &[u8], control: &ExecutionControl<'_>) -> QueryResult<bool> {
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.chunks(64 * 1024).zip(right.chunks(64 * 1024)) {
        control.bytes(left.len() as u64)?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

fn contains_controlled(
    left: &[u8],
    right: &[u8],
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if right.is_empty() {
        return Ok(true);
    }
    if right.len() > left.len() {
        return Ok(false);
    }
    let _memory = control.reserve(allocation_bytes::<usize>(right.len())?)?;
    let prefix = prefix_table(right, control)?;
    let mut matched = 0;
    for value in left {
        control.work(1)?;
        while matched > 0 && *value != right[matched] {
            control.work(1)?;
            matched = prefix[matched - 1];
        }
        if *value == right[matched] {
            matched += 1;
        }
        if matched == right.len() {
            return Ok(true);
        }
    }
    Ok(false)
}

fn prefix_table(pattern: &[u8], control: &ExecutionControl<'_>) -> QueryResult<Vec<usize>> {
    let mut prefix = Vec::with_capacity(pattern.len());
    prefix.push(0);
    let mut matched = 0;
    for index in 1..pattern.len() {
        control.work(1)?;
        while matched > 0 && pattern[index] != pattern[matched] {
            control.work(1)?;
            matched = prefix[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        prefix.push(matched);
    }
    Ok(prefix)
}

fn regex_predicate_controlled(
    left: Value,
    right: Value,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    let (Value::String(left), Value::String(pattern)) = (left, right) else {
        return Ok(Value::Null);
    };
    let size = pattern.len().checked_add(6).ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::Resource,
            "regular expression length overflow",
        )
    })?;
    let _pattern_memory = control.reserve(size as u64)?;
    let mut anchored = String::with_capacity(size);
    anchored.push_str("^(?:");
    crate::query::functions::append_text_controlled(&mut anchored, &pattern, Some(control))?;
    anchored.push_str(")$");
    let (expression, _compiled_memory) =
        crate::query::functions::compile_regex_controlled(&anchored, Some(control))?;
    control.check()?;
    let matched = expression.is_match(&left);
    control.check()?;
    Ok(Value::Boolean(matched))
}

fn in_operator(
    left: Value,
    right: Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    if matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::List(values) = right else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "IN right operand must be a List or null",
        ));
    };
    if values.is_empty() {
        return Ok(Value::Boolean(false));
    }
    if matches!(left, Value::Null) {
        return Ok(Value::Null);
    }
    let mut unknown = false;
    for value in values {
        match cypher_equals_controlled(&left, &value, control)? {
            Some(true) => return Ok(Value::Boolean(true)),
            Some(false) => {}
            None => unknown = true,
        }
    }
    Ok(if unknown {
        Value::Null
    } else {
        Value::Boolean(false)
    })
}

fn string_predicate(op: BinaryOp, left: Value, right: Value) -> QueryResult<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::String(left), Value::String(right)) = (left, right) else {
        return Ok(Value::Null);
    };
    let value = match op {
        BinaryOp::StartsWith => left.starts_with(&right),
        BinaryOp::EndsWith => left.ends_with(&right),
        BinaryOp::Contains => left.contains(&right),
        _ => return Err(QueryError::internal("invalid String predicate operator")),
    };
    Ok(Value::Boolean(value))
}

fn regex_predicate(left: Value, right: Value) -> QueryResult<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::String(left), Value::String(pattern)) = (left, right) else {
        return Ok(Value::Null);
    };
    let pattern = format!("^(?:{pattern})$");
    let expression = regex::Regex::new(&pattern)
        .map_err(|error| QueryError::semantic(format!("invalid regular expression: {error}")))?;
    Ok(Value::Boolean(expression.is_match(&left)))
}

fn boolean_binary(op: BinaryOp, left: Value, right: Value) -> QueryResult<Value> {
    let left = bool_or_null(left)?;
    let right = bool_or_null(right)?;
    let value = match op {
        BinaryOp::And => match (left, right) {
            (Some(false), _) | (_, Some(false)) => Some(false),
            (Some(true), Some(true)) => Some(true),
            _ => None,
        },
        BinaryOp::Or => match (left, right) {
            (Some(true), _) | (_, Some(true)) => Some(true),
            (Some(false), Some(false)) => Some(false),
            _ => None,
        },
        BinaryOp::Xor => match (left, right) {
            (Some(left), Some(right)) => Some(left ^ right),
            _ => None,
        },
        _ => None,
    };
    Ok(value.map(Value::Boolean).unwrap_or(Value::Null))
}

fn bool_or_null(value: Value) -> QueryResult<Option<bool>> {
    match value {
        Value::Boolean(value) => Ok(Some(value)),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "Boolean operator requires Boolean or null",
        )),
    }
}

fn ordered_binary(op: BinaryOp, left: &Value, right: &Value) -> QueryResult<Value> {
    let Some(comparison) = cypher::cypher_compare(left, right)? else {
        return Ok(Value::Null);
    };
    let value = match comparison {
        CypherComparison::Unordered => false,
        CypherComparison::Less => matches!(op, BinaryOp::Less | BinaryOp::LessEqual),
        CypherComparison::Equal => matches!(op, BinaryOp::LessEqual | BinaryOp::GreaterEqual),
        CypherComparison::Greater => matches!(op, BinaryOp::Greater | BinaryOp::GreaterEqual),
    };
    Ok(Value::Boolean(value))
}

fn numeric_binary(op: BinaryOp, left: Value, right: Value) -> QueryResult<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    if let Some(result) = crate::query::functions::evaluate_temporal_arithmetic(op, &left, &right) {
        return result;
    }
    match (left, right) {
        (Value::Integer(left), Value::Integer(right)) if op != BinaryOp::Power => {
            integer_binary(op, left, right)
        }
        (Value::Integer(left), Value::Integer(right)) => {
            float_binary(op, left as f64, right as f64)
        }
        (Value::Integer(left), Value::Float(right)) => float_binary(op, left as f64, right),
        (Value::Float(left), Value::Integer(right)) => float_binary(op, left, right as f64),
        (Value::Float(left), Value::Float(right)) => float_binary(op, left, right),
        (Value::String(left), Value::String(right)) if op == BinaryOp::Add => {
            Ok(Value::String(left + &right))
        }
        (Value::String(left), Value::String(right)) if op == BinaryOp::Concat => {
            Ok(Value::String(left + &right))
        }
        (Value::List(mut left), Value::List(right))
            if matches!(op, BinaryOp::Add | BinaryOp::Concat) =>
        {
            left.extend(right);
            Ok(Value::List(left))
        }
        (Value::List(mut left), right) if op == BinaryOp::Add => {
            left.push(right);
            Ok(Value::List(left))
        }
        (left, Value::List(mut right)) if op == BinaryOp::Add => {
            right.insert(0, left);
            Ok(Value::List(right))
        }
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "arithmetic operator received incompatible values",
        )),
    }
}

fn integer_binary(op: BinaryOp, left: i64, right: i64) -> QueryResult<Value> {
    let value = match op {
        BinaryOp::Add => left.checked_add(right),
        BinaryOp::Subtract => left.checked_sub(right),
        BinaryOp::Multiply => left.checked_mul(right),
        BinaryOp::Divide if right != 0 => left.checked_div(right),
        BinaryOp::Divide => {
            return Err(QueryError::new(QueryErrorKind::Type, "division by zero"));
        }
        BinaryOp::Modulo if right != 0 => left.checked_rem(right),
        BinaryOp::Modulo => {
            return Err(QueryError::new(QueryErrorKind::Type, "division by zero"));
        }
        _ => None,
    };
    value
        .map(Value::Integer)
        .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "INTEGER64 arithmetic overflow"))
}

fn float_binary(op: BinaryOp, left: f64, right: f64) -> QueryResult<Value> {
    let value = match op {
        BinaryOp::Add => left + right,
        BinaryOp::Subtract => left - right,
        BinaryOp::Multiply => left * right,
        BinaryOp::Divide => left / right,
        BinaryOp::Modulo => left % right,
        BinaryOp::Power => left.powf(right),
        _ => {
            return Err(QueryError::internal(
                "non-arithmetic operator reached Float evaluation",
            ));
        }
    };
    Ok(Value::Float(value))
}

#[cfg(test)]
mod controlled_tests {
    use std::cell::Cell;
    use std::collections::BTreeMap;

    use super::*;
    use crate::query::resources::QueryResources;

    #[test]
    fn in_operator_cancels_a_single_nested_comparison() {
        let left = Value::Map(BTreeMap::from([(
            "nested".into(),
            Value::List((0..20_000).map(Value::Integer).collect()),
        )]));
        let right = Value::List(vec![left.clone()]);
        let checks = Cell::new(0);
        let interrupt = || {
            checks.set(checks.get() + 1);
            checks.get() >= 4
        };
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &interrupt);
        assert_eq!(
            evaluate_binary_controlled(BinaryOp::In, left, right, Some(&control))
                .unwrap_err()
                .kind,
            QueryErrorKind::Interrupted
        );
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn string_predicates_match_utf8_and_cross_chunk_boundaries() {
        let mut long = "a".repeat(65_535);
        long.push_str("中文b");
        let resources = QueryResources::default();
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        for input in ["", "中文", "aaabaaba", &long] {
            for search in ["", "中", "文", "aaab", "a中文b", "other"] {
                for operation in [BinaryOp::Contains, BinaryOp::StartsWith, BinaryOp::EndsWith] {
                    let left = Value::String(input.into());
                    let right = Value::String(search.into());
                    let expected = evaluate_binary(operation, left.clone(), right.clone()).unwrap();
                    assert_eq!(
                        evaluate_binary_controlled(operation, left, right, Some(&control)).unwrap(),
                        expected
                    );
                }
            }
        }
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn contains_checks_during_prefix_construction_and_haystack_scan() {
        for (input, search) in [
            ("a".repeat(100_000), "b".into()),
            ("a".repeat(100_000), "a".repeat(50_000)),
        ] {
            let checks = Cell::new(0);
            let interrupt = || {
                checks.set(checks.get() + 1);
                checks.get() >= 4
            };
            let resources = QueryResources::default();
            let control = ExecutionControl::new(&resources, &interrupt);
            assert_eq!(
                evaluate_binary_controlled(
                    BinaryOp::Contains,
                    Value::String(input),
                    Value::String(search),
                    Some(&control)
                )
                .unwrap_err()
                .kind,
                QueryErrorKind::Interrupted
            );
            assert_eq!(resources.used(), 0);
        }
    }

    #[test]
    fn regex_operator_reserves_wrapper_and_limits_compiled_state() {
        let never = || false;
        for pattern in ["x".repeat(8192), "[a-z]{100000}".into()] {
            let resources = QueryResources::new(Some(4096));
            let control = ExecutionControl::new(&resources, &never);
            assert_eq!(
                evaluate_binary_controlled(
                    BinaryOp::Regex,
                    Value::String("abc".into()),
                    Value::String(pattern),
                    Some(&control)
                )
                .unwrap_err()
                .kind,
                QueryErrorKind::Resource
            );
            assert_eq!(resources.used(), 0);
        }
        let resources = QueryResources::new(Some(1024 * 1024));
        let control = ExecutionControl::new(&resources, &never);
        assert_eq!(
            evaluate_binary_controlled(
                BinaryOp::Regex,
                Value::String("abc".into()),
                Value::String("a.c".into()),
                Some(&control)
            )
            .unwrap(),
            Value::Boolean(true)
        );
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn concatenation_transfers_an_exact_output_reservation() {
        let resources = QueryResources::new(Some(1024 * 1024));
        let never = || false;
        let control = ExecutionControl::new(&resources, &never);
        let mut memory = None;
        let value = evaluate_binary_with_memory(
            BinaryOp::Add,
            Value::List(vec![Value::String("a".repeat(100))]),
            Value::Integer(2),
            Some(&control),
            &mut memory,
        )
        .unwrap();
        let Value::List(values) = &value else {
            panic!("expected List");
        };
        assert_eq!(values.capacity(), values.len());
        assert!(resources.used() >= crate::query::resources::value_bytes(&value).unwrap());
        assert_eq!(memory.as_ref().unwrap().bytes(), resources.used());
        drop(value);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }
}
