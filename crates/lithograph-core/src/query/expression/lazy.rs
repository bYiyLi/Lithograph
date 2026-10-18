use crate::cypher::Value;
use crate::query::resources::allocation_bytes;
use crate::query::{ExecutionControl, QueryError, QueryErrorKind, QueryResult};

#[cfg(test)]
use super::controlled::reserve_value;
use super::evaluate::EvaluatedValue;
use super::operators::cypher_equals_controlled;
use super::{BinaryOp, Expr};

pub(crate) fn evaluate_lazy_expression_with_memory(
    expression: &Expr,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<EvaluatedValue>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<EvaluatedValue>> {
    control.work(1)?;
    match expression {
        Expr::Case {
            operand,
            alternatives,
            fallback,
        } => evaluate_case_with_memory(
            operand.as_deref(),
            alternatives,
            fallback.as_deref(),
            evaluate,
            control,
        )
        .map(Some),
        Expr::Binary(BinaryOp::And, left, right) => {
            evaluate_lazy_boolean_with_memory(left, right, false, evaluate).map(Some)
        }
        Expr::Binary(BinaryOp::Or, left, right) => {
            evaluate_lazy_boolean_with_memory(left, right, true, evaluate).map(Some)
        }
        _ => Ok(None),
    }
}

fn evaluate_case_with_memory(
    operand: Option<&Expr>,
    alternatives: &[(Expr, Expr)],
    fallback: Option<&Expr>,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<EvaluatedValue>,
    control: &ExecutionControl<'_>,
) -> QueryResult<EvaluatedValue> {
    let operand = operand.map(&mut *evaluate).transpose()?;
    for (condition, result) in alternatives {
        control.work(1)?;
        let condition = evaluate(condition)?;
        let take = case_condition(
            operand.as_ref().map(|operand| &operand.value),
            &condition.value,
            Some(control),
        )?;
        if take {
            return evaluate(result);
        }
    }
    if let Some(fallback) = fallback {
        evaluate(fallback)
    } else {
        let reservation = control.reserve(allocation_bytes::<Value>(1)?)?;
        Ok(EvaluatedValue {
            value: Value::Null,
            reservation,
        })
    }
}

fn case_condition(
    operand: Option<&Value>,
    condition: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    if let Some(operand) = operand {
        return cypher_equals_controlled(operand, condition, control)
            .map(|value| value.unwrap_or(false));
    }
    match condition {
        Value::Boolean(value) => Ok(*value),
        Value::Null => Ok(false),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "searched CASE condition must be Boolean or null",
        )),
    }
}

fn evaluate_lazy_boolean_with_memory(
    left: &Expr,
    right: &Expr,
    short_value: bool,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<EvaluatedValue>,
) -> QueryResult<EvaluatedValue> {
    let left = evaluate(left)?;
    let left_is_null = matches!(left.value, Value::Null);
    match left.value {
        Value::Boolean(value) if value == short_value => return Ok(left),
        Value::Boolean(_) | Value::Null => {}
        _ => return boolean_operand_error(),
    }
    let mut right = evaluate(right)?;
    match right.value {
        Value::Boolean(value) if value == short_value => {}
        Value::Boolean(_) | Value::Null if left_is_null => right.value = Value::Null,
        Value::Boolean(_) | Value::Null => {}
        _ => return boolean_operand_error(),
    }
    Ok(right)
}

#[cfg(test)]
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

#[cfg(test)]
pub(super) fn evaluate_case(
    operand: Option<&Expr>,
    alternatives: &[(Expr, Expr)],
    fallback: Option<&Expr>,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<Value>,
) -> QueryResult<Value> {
    evaluate_case_controlled(operand, alternatives, fallback, evaluate, None)
}

#[cfg(test)]
pub(super) fn evaluate_case_controlled(
    operand: Option<&Expr>,
    alternatives: &[(Expr, Expr)],
    fallback: Option<&Expr>,
    evaluate: &mut impl FnMut(&Expr) -> QueryResult<Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    if let Some(control) = control {
        control.check()?;
    }
    let operand = operand.map(&mut *evaluate).transpose()?;
    let _operand_memory = operand
        .as_ref()
        .map(|operand| reserve_value(operand, control))
        .transpose()?;
    for (condition, result) in alternatives {
        if let Some(control) = control {
            control.work(1)?;
        }
        let condition = evaluate(condition)?;
        let _condition_memory = reserve_value(&condition, control)?;
        let take = case_condition(operand.as_ref(), &condition, control)?;
        if take {
            return evaluate(result);
        }
    }
    fallback.map_or(Ok(Value::Null), evaluate)
}

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
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

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use rusqlite::Connection;

    use crate::query::{ExecutionOptions, QueryCursor, QueryResources, prepare};
    use crate::storage::{create_storage_schema, initialize_root};

    use super::*;

    fn storage() -> Connection {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), \
                 magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);",
            )
            .unwrap();
        create_storage_schema(&connection).unwrap();
        initialize_root(&connection).unwrap();
        connection
    }

    fn execute_case(
        connection: &Connection,
        query: &str,
        limit: Option<u64>,
    ) -> QueryResult<Vec<Vec<Value>>> {
        let prepared = prepare(
            connection,
            query,
            BTreeMap::new(),
            ExecutionOptions {
                memory_limit_bytes: limit,
                ..ExecutionOptions::default()
            },
        )?;
        let mut cursor = QueryCursor::new(prepared);
        let mut rows = Vec::new();
        loop {
            let batch = cursor.next_batch(connection, 1)?;
            rows.extend(batch.rows);
            if batch.done {
                cursor.complete(connection)?;
                return Ok(rows);
            }
        }
    }

    #[test]
    fn case_operand_and_condition_share_one_live_memory_budget() {
        let connection = storage();
        let query = "RETURN CASE range(1,90000) WHEN range(1,90000) THEN 1 ELSE 0 END AS result";
        let error = execute_case(&connection, query, Some(16 * 1024 * 1024)).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
        for limit in [None, Some(256 * 1024 * 1024)] {
            assert_eq!(
                execute_case(&connection, query, limit).unwrap(),
                vec![vec![Value::Integer(1)]]
            );
        }
    }

    #[test]
    fn graph_case_holds_operand_budget_and_releases_each_nonmatching_condition() {
        let connection = storage();
        let query = "RETURN CASE range(1,90000) WHEN range(1,90000) \
                     THEN COUNT { RETURN 1 } ELSE 0 END AS result";
        let error = execute_case(&connection, query, Some(16 * 1024 * 1024)).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
        assert_eq!(
            execute_case(&connection, query, Some(256 * 1024 * 1024)).unwrap(),
            vec![vec![Value::Integer(1)]]
        );
        let distinct = "RETURN CASE range(1,90000) WHEN range(2,90001) \
                        THEN COUNT { RETURN 1 } WHEN range(1,90000) THEN 2 ELSE 0 END AS result";
        assert_eq!(
            execute_case(&connection, distinct, Some(32 * 1024 * 1024)).unwrap(),
            vec![vec![Value::Integer(2)]]
        );
    }

    #[test]
    fn tracked_lazy_booleans_keep_truth_tables_and_transfer_the_result_claim() {
        let values = [Value::Boolean(false), Value::Boolean(true), Value::Null];
        for op in [BinaryOp::And, BinaryOp::Or] {
            for left in &values {
                for right in &values {
                    assert_tracked_boolean(op, left, right);
                }
            }
        }
    }

    fn assert_tracked_boolean(op: BinaryOp, left: &Value, right: &Value) {
        let expression = Expr::Binary(
            op,
            Box::new(Expr::Literal(left.clone())),
            Box::new(Expr::Literal(right.clone())),
        );
        let resources = QueryResources::new(Some(1_024));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let actual = evaluate_lazy_expression_with_memory(
            &expression,
            &mut |expression| tracked_literal(expression, &control),
            &control,
        )
        .unwrap()
        .unwrap();
        let expected = evaluate_lazy_expression(&expression, &mut |expression| match expression {
            Expr::Literal(value) => Ok(value.clone()),
            _ => unreachable!(),
        })
        .unwrap()
        .unwrap();
        assert_eq!(actual.value, expected, "{left:?} {op:?} {right:?}");
        assert_eq!(resources.used(), allocation_bytes::<Value>(1).unwrap());
        drop(actual);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn tracked_lazy_boolean_short_circuits_before_the_other_expression() {
        for (op, short_value) in [(BinaryOp::And, false), (BinaryOp::Or, true)] {
            let resources = QueryResources::new(Some(1_024));
            let interrupted = || false;
            let control = ExecutionControl::new(&resources, &interrupted);
            let expression = Expr::Binary(
                op,
                Box::new(Expr::Literal(Value::Boolean(short_value))),
                Box::new(Expr::Variable("must_not_evaluate".to_owned())),
            );
            let actual = evaluate_lazy_expression_with_memory(
                &expression,
                &mut |expression| tracked_literal(expression, &control),
                &control,
            )
            .unwrap()
            .unwrap();
            assert_eq!(actual.value, Value::Boolean(short_value));
        }
    }

    fn tracked_literal(
        expression: &Expr,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<EvaluatedValue> {
        let Expr::Literal(value) = expression else {
            panic!("unselected expression was evaluated");
        };
        let reservation = control.reserve(allocation_bytes::<Value>(1)?)?;
        Ok(EvaluatedValue {
            value: value.clone(),
            reservation,
        })
    }

    #[test]
    fn case_keeps_unselected_result_expressions_lazy_under_a_memory_limit() {
        let connection = storage();
        for query in [
            "RETURN CASE WHEN false THEN range(1,500000) ELSE 1 END AS result",
            "RETURN CASE 1 WHEN 1 THEN 1 ELSE range(1,500000) END AS result",
            "RETURN CASE 1 WHEN 1 THEN 1 WHEN size(range(1,500000)) THEN 2 END AS result",
        ] {
            assert_eq!(
                execute_case(&connection, query, Some(16 * 1024 * 1024)).unwrap(),
                vec![vec![Value::Integer(1)]],
                "{query}"
            );
        }
    }

    #[test]
    fn controlled_case_releases_operand_and_condition_claims_on_budget_error() {
        let resources = QueryResources::new(Some(1_024));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let operand = Expr::Literal(Value::String("x".repeat(700)));
        let alternatives = [(
            Expr::Literal(Value::String("x".repeat(700))),
            Expr::Literal(Value::Integer(1)),
        )];
        let error = evaluate_case_controlled(
            Some(&operand),
            &alternatives,
            None,
            &mut |expression| match expression {
                Expr::Literal(value) => Ok(value.clone()),
                _ => unreachable!(),
            },
            Some(&control),
        )
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.used(), 0);
    }
}
