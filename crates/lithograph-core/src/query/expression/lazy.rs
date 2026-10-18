use crate::cypher::{self, Value};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

use super::{BinaryOp, Expr};

pub(crate) fn evaluate_lazy_expression(
    expression: &Expr,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<Value>,
) -> QueryResult<Option<Value>> {
    match expression {
        Expr::Case {
            operand,
            alternatives,
            fallback,
        } => evaluate_case(
            operand.as_deref(),
            alternatives,
            fallback.as_deref(),
            evaluate,
        )
        .map(Some),
        Expr::Binary(BinaryOp::And, left, right) => {
            evaluate_lazy_and(left, right, evaluate).map(Some)
        }
        Expr::Binary(BinaryOp::Or, left, right) => {
            evaluate_lazy_or(left, right, evaluate).map(Some)
        }
        _ => Ok(None),
    }
}

pub(super) fn evaluate_case(
    operand: Option<&Expr>,
    alternatives: &[(Expr, Expr)],
    fallback: Option<&Expr>,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<Value>,
) -> QueryResult<Value> {
    let operand = operand.map(&mut *evaluate).transpose()?;
    for (condition, result) in alternatives {
        let condition = evaluate(condition)?;
        let take = match &operand {
            Some(operand) => cypher::cypher_equals(operand, &condition)?.unwrap_or(false),
            None => match condition {
                Value::Boolean(value) => value,
                Value::Null => false,
                _ => {
                    return Err(QueryError::new(
                        QueryErrorKind::Type,
                        "searched CASE condition must be Boolean or null",
                    ));
                }
            },
        };
        if take {
            return evaluate(result);
        }
    }
    fallback.map_or(Ok(Value::Null), evaluate)
}

pub(super) fn evaluate_lazy_and(
    left: &Expr,
    right: &Expr,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<Value>,
) -> QueryResult<Value> {
    match evaluate(left)? {
        Value::Boolean(false) => Ok(Value::Boolean(false)),
        Value::Boolean(true) => require_boolean_or_null(evaluate(right)?),
        Value::Null => match require_boolean_or_null(evaluate(right)?)? {
            Value::Boolean(false) => Ok(Value::Boolean(false)),
            Value::Boolean(true) | Value::Null => Ok(Value::Null),
            _ => unreachable!("boolean-or-null validator returned another value"),
        },
        _ => boolean_operand_error(),
    }
}

pub(super) fn evaluate_lazy_or(
    left: &Expr,
    right: &Expr,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<Value>,
) -> QueryResult<Value> {
    match evaluate(left)? {
        Value::Boolean(true) => Ok(Value::Boolean(true)),
        Value::Boolean(false) => require_boolean_or_null(evaluate(right)?),
        Value::Null => match require_boolean_or_null(evaluate(right)?)? {
            Value::Boolean(true) => Ok(Value::Boolean(true)),
            Value::Boolean(false) | Value::Null => Ok(Value::Null),
            _ => unreachable!("boolean-or-null validator returned another value"),
        },
        _ => boolean_operand_error(),
    }
}

fn require_boolean_or_null(value: Value) -> QueryResult<Value> {
    if matches!(value, Value::Boolean(_) | Value::Null) {
        Ok(value)
    } else {
        boolean_operand_error()
    }
}

fn boolean_operand_error<T>() -> QueryResult<T> {
    Err(QueryError::new(
        QueryErrorKind::Type,
        "Boolean operator requires Boolean or null",
    ))
}
