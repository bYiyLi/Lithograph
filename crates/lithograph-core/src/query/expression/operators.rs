use crate::cypher::{self, CypherComparison, Value};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

use super::{BinaryOp, UnaryOp};

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
        BinaryOp::In => in_operator(left, right),
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

fn in_operator(left: Value, right: Value) -> QueryResult<Value> {
    if matches!(left, Value::Null) || matches!(right, Value::Null) {
        return Ok(Value::Null);
    }
    let Value::List(values) = right else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "IN right operand must be a List or null",
        ));
    };
    let mut unknown = false;
    for value in values {
        match cypher::cypher_equals(&left, &value)? {
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
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "String predicate requires two String operands",
        ));
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
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "=~ requires two String operands",
        ));
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
        (Value::Integer(left), Value::Integer(right))
            if op != BinaryOp::Divide && op != BinaryOp::Power =>
        {
            integer_binary(op, left, right)
        }
        (Value::Integer(_), Value::Integer(0)) if op == BinaryOp::Divide => {
            Err(QueryError::new(QueryErrorKind::Type, "division by zero"))
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
