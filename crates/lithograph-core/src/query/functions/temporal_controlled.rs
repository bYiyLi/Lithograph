//! Temporal function dispatch with execution-owned resource accounting.

use crate::cypher::Value;
use crate::query::QueryResult;
use crate::query::expression::value_bytes_with_control;
use crate::query::resources::{ExecutionControl, MemoryReservation, checked_add, checked_mul};

use super::super::{contains_null, require_arity};
use super::{
    TemporalFamily, controlled_pattern, format_instant, format_instant_token, instant_parts,
    temporal_type_error, truncate,
};

pub(crate) fn evaluate_with_memory(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    if name == "format" {
        return Some(format_value_controlled(values, control, result_memory));
    }
    let family = match name {
        "date.truncate" => TemporalFamily::Date,
        "localtime.truncate" => TemporalFamily::LocalTime,
        "time.truncate" => TemporalFamily::Time,
        "localdatetime.truncate" => TemporalFamily::LocalDateTime,
        "datetime.truncate" => TemporalFamily::ZonedDateTime,
        _ => return None,
    };
    Some(truncate_with_memory(values, family, control, result_memory))
}

fn truncate_with_memory(
    values: &[Value],
    family: TemporalFamily,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let input = values.iter().try_fold(0, |bytes, value| {
        checked_add(bytes, value_bytes_with_control(value, control)?)
    })?;
    let bytes = checked_add(checked_mul(input, 4)?, 1024)?;
    let mut memory = control.map(|control| control.reserve(bytes)).transpose()?;
    let value = truncate(values, family, control)?;
    if let Some(control) = control {
        control.check()?;
    }
    if let Some(memory) = &mut memory {
        memory.resize(value_bytes_with_control(&value, control)?)?;
    }
    *result_memory = memory;
    Ok(value)
}

fn format_arguments(values: &[Value]) -> QueryResult<Option<(&Value, Option<&str>)>> {
    require_arity(values, 1, 2)?;
    if contains_null(values) {
        return Ok(None);
    }
    let pattern = match values.get(1) {
        None => None,
        Some(Value::String(pattern)) => Some(pattern.as_str()),
        Some(_) => return temporal_type_error("format pattern must be String"),
    };
    Ok(Some((&values[0], pattern)))
}

pub(super) fn format_value(values: &[Value]) -> QueryResult<Value> {
    let Some((value, pattern)) = format_arguments(values)? else {
        return Ok(Value::Null);
    };
    let output = match (value, pattern) {
        (value, None) => {
            let (base, zone) = default_format_parts(value)?;
            match zone {
                Some(zone) => format!("{base}[{zone}]"),
                None => base.to_owned(),
            }
        }
        (Value::Duration(value), Some(pattern)) => {
            super::super::duration_format::format_duration(value, pattern)?
        }
        (value, Some(pattern)) => format_instant(value, pattern)?,
    };
    Ok(Value::String(output))
}

fn format_value_controlled(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let Some((value, pattern)) = format_arguments(values)? else {
        return Ok(Value::Null);
    };
    let Some(pattern) = pattern else {
        return format_default_controlled(value, control, result_memory);
    };
    let output = if let Value::Duration(value) = value {
        super::super::duration_format::format_duration_controlled(
            value,
            pattern,
            control,
            result_memory,
        )?
    } else {
        let parts = instant_parts(value)?;
        let temporary = checked_add(128, parts.zone.map_or(0, |zone| zone.len() as u64))?;
        controlled_pattern::render(
            pattern,
            control,
            temporary,
            result_memory,
            |character, count| {
                let numeric = matches!(
                    character,
                    'u' | 'y'
                        | 'Y'
                        | 'd'
                        | 'D'
                        | 'g'
                        | 'F'
                        | 'W'
                        | 'w'
                        | 'H'
                        | 'k'
                        | 'K'
                        | 'h'
                        | 'm'
                        | 's'
                        | 'n'
                        | 'N'
                        | 'A'
                );
                let width = if numeric && count > 2 { 1 } else { count };
                Ok((format_instant_token(parts, character, width)?, numeric))
            },
        )?
    };
    Ok(Value::String(output))
}

fn format_default_controlled(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let (base, zone) = default_format_parts(value)?;
    let suffix = zone
        .map(|zone| checked_add(zone.len() as u64, 2))
        .transpose()?
        .unwrap_or(0);
    let length = checked_add(base.len() as u64, suffix)?;
    let bytes = checked_add(std::mem::size_of::<Value>() as u64, length)?;
    let memory = control.map(|control| control.reserve(bytes)).transpose()?;
    let mut output =
        String::with_capacity(usize::try_from(length).map_err(|_| {
            crate::query::resources::memory_error("temporal format length overflow")
        })?);
    controlled_pattern::append(&mut output, base, 1, control)?;
    if let Some(zone) = zone {
        output.push('[');
        controlled_pattern::append(&mut output, zone, 1, control)?;
        output.push(']');
    }
    if let Some(control) = control {
        control.check()?;
    }
    *result_memory = memory;
    Ok(Value::String(output))
}

fn default_format_parts(value: &Value) -> QueryResult<(&str, Option<&str>)> {
    let base = match value {
        Value::Date(value) => value.as_str(),
        Value::LocalTime(value) => value.as_str(),
        Value::Time(value) => value.as_str(),
        Value::LocalDateTime(value) => value.as_str(),
        Value::Duration(value) => value.as_str(),
        Value::ZonedDateTime(value) => {
            let zone = value.zone();
            let suffix = if zone == "Z" || zone.starts_with('+') || zone.starts_with('-') {
                None
            } else {
                Some(zone)
            };
            return Ok((value.value(), suffix));
        }
        _ => return temporal_type_error("format input must be a temporal value"),
    };
    Ok((base, None))
}
