//! Runtime implementations for the frozen current-graph built-in registry.

use crate::cypher::Value;

use super::expression::value_bytes_with_control;
use super::resources::{ExecutionControl, MemoryReservation, checked_add, checked_mul};
use super::{QueryError, QueryResult};

// The legacy dispatch and focused function tests use the same unchecked-output
// adapter. Production dispatch uses evaluate_with_memory to transfer its lease.
macro_rules! legacy_evaluators {
    () => {
        pub(super) fn evaluate(
            name: &str,
            values: &[$crate::cypher::Value],
        ) -> Option<$crate::query::QueryResult<$crate::cypher::Value>> {
            evaluate_controlled(name, values, None)
        }

        pub(super) fn evaluate_controlled(
            name: &str,
            values: &[$crate::cypher::Value],
            control: Option<&$crate::query::resources::ExecutionControl<'_>>,
        ) -> Option<$crate::query::QueryResult<$crate::cypher::Value>> {
            evaluate_with_memory(name, values, control, &mut None)
        }
    };
}
pub(super) use legacy_evaluators;

fn build_value(
    bytes: u64,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
    build: impl FnOnce() -> QueryResult<Value>,
) -> QueryResult<Value> {
    let memory = control.map(|control| control.reserve(bytes)).transpose()?;
    let value = build()?;
    *result_memory = memory;
    Ok(value)
}

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
pub(crate) use text::{append as append_text_controlled, find as find_text_controlled};
pub(crate) use text::{compile_regex_controlled, normalized_predicate_controlled};

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let name = name.to_ascii_lowercase();
    numeric::evaluate(&name, values)
        .or_else(|| text::evaluate(&name, values))
        .or_else(|| collection::evaluate(&name, values))
        .or_else(|| conversion::evaluate(&name, values))
        .or_else(|| structural::evaluate(&name, values))
        .or_else(|| typed::evaluate(&name, values))
        .or_else(|| temporal::evaluate(&name, values))
        .or_else(|| instant::evaluate(&name, values))
}

pub(super) fn evaluate_with_memory(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    let _name_memory = match control
        .map(|control| control.reserve(name.len() as u64))
        .transpose()
    {
        Ok(memory) => memory,
        Err(error) => return Some(Err(error)),
    };
    let name = name.to_ascii_lowercase();
    numeric::evaluate(&name, values)
        .or_else(|| text::evaluate_with_memory(&name, values, control, memory))
        .or_else(|| collection::evaluate_with_memory(&name, values, control, memory))
        .or_else(|| conversion::evaluate_with_memory(&name, values, control, memory))
        .or_else(|| structural::evaluate_with_memory(&name, values, control, memory))
        .or_else(|| typed::evaluate_with_memory(&name, values, control, memory))
        .or_else(|| temporal::evaluate_with_memory(&name, values, control, memory))
        .or_else(|| evaluate_temporal_with_memory(&name, values, control, memory))
}

fn evaluate_temporal_with_memory(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    let reservation = (|| {
        let Some(control) = control else {
            return Ok(None);
        };
        control.check()?;
        let input = values.iter().try_fold(0, |sum, value| {
            checked_add(sum, value_bytes_with_control(value, Some(control))?)
        })?;
        control
            .reserve(checked_add(checked_mul(input, 4)?, 1024)?)
            .map(Some)
    })();
    let mut reservation = match reservation {
        Ok(value) => value,
        Err(error) => return Some(Err(error)),
    };
    let result = temporal::evaluate(name, values).or_else(|| instant::evaluate(name, values))?;
    Some(result.and_then(|value| {
        if let Some(control) = control {
            control.check()?;
        }
        if let Some(reservation) = &mut reservation {
            reservation.resize(value_bytes_with_control(&value, control)?)?;
        }
        *memory = reservation;
        Ok(value)
    }))
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
