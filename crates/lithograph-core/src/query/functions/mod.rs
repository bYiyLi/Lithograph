//! Runtime implementations for the frozen current-graph built-in registry.

use crate::cypher::Value;

use super::{QueryError, QueryResult};

mod collection;
mod conversion;
mod duration_format;
mod format_components;
mod instant;
mod numeric;
mod structural;
mod temporal;
mod text;
mod typed;

pub(crate) use instant::{install_statement_time, install_transaction_time};

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let name = name.to_ascii_lowercase();
    numeric::evaluate(&name, values)
        .or_else(|| text::evaluate(&name, values))
        .or_else(|| collection::evaluate(&name, values))
        .or_else(|| conversion::evaluate(&name, values))
        .or_else(|| structural::evaluate(&name, values))
        .or_else(|| temporal::evaluate(&name, values))
        .or_else(|| instant::evaluate(&name, values))
        .or_else(|| typed::evaluate(&name, values))
}

pub(crate) use temporal::evaluate_arithmetic as evaluate_temporal_arithmetic;

fn require_arity(values: &[Value], minimum: usize, maximum: usize) -> QueryResult<()> {
    if (minimum..=maximum).contains(&values.len()) {
        Ok(())
    } else {
        Err(QueryError::semantic(format!(
            "function expects between {minimum} and {maximum} arguments"
        )))
    }
}

fn require_exact_arity(values: &[Value], expected: usize) -> QueryResult<()> {
    if values.len() == expected {
        Ok(())
    } else {
        Err(QueryError::semantic(format!(
            "function expects {expected} argument(s)"
        )))
    }
}

fn contains_null(values: &[Value]) -> bool {
    values.iter().any(|value| matches!(value, Value::Null))
}

pub(super) fn element_id_value(value: &Value) -> QueryResult<Value> {
    structural::element_id_value(value)
}
