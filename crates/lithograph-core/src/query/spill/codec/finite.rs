//! Spill-only Float protection around the existing checked Value JSON codec.
//! Prefix escaping is injective for every JSON string, including graph fields,
//! Map.entries, properties and binding metadata. No object is reinterpreted as
//! a private type and no extra nesting is added. Caller serialization/decode
//! leases cover the original DOM and final text. A separate checked preflight
//! holds marker/escaping scratch until the transformed DOM is released.

use super::*;

const PREFIX: &str = "~";
const FLOAT: &str = "~F";
const TEXT: &str = "~S";

pub(super) fn encoding_bytes(
    value: &JsonValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    work(control)?;
    match value {
        JsonValue::Number(number) if number.is_f64() => Ok(((FLOAT.len() + 16) * 4 + 16) as u64),
        JsonValue::String(string) if string.starts_with(PREFIX) => {
            escaped_strings_bytes(string.len() as u64, 1)
        }
        JsonValue::Array(values) => values.iter().try_fold(0, |total, value| {
            checked_add(total, encoding_bytes(value, control)?)
        }),
        JsonValue::Object(values) => values.values().try_fold(0, |total, value| {
            checked_add(total, encoding_bytes(value, control)?)
        }),
        _ => Ok(0),
    }
}

pub(super) fn escaped_strings_bytes(bytes: u64, strings: u64) -> QueryResult<u64> {
    checked_add(
        checked_mul(
            checked_add(bytes, checked_mul(strings, TEXT.len() as u64)?)?,
            2,
        )?,
        checked_mul(strings, 32)?,
    )
}

pub(super) fn encode(
    value: &mut JsonValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    work(control)?;
    match value {
        JsonValue::Number(number) if number.is_f64() => {
            let number = number
                .as_f64()
                .ok_or_else(|| QueryError::internal("invalid spill Float"))?;
            let mut encoded = String::with_capacity(FLOAT.len() + 16);
            use std::fmt::Write;
            write!(&mut encoded, "{FLOAT}{:016x}", number.to_bits())
                .map_err(|_| QueryError::internal("failed to format spill Float bits"))?;
            *value = JsonValue::String(encoded);
        }
        JsonValue::String(string) if string.starts_with(PREFIX) => {
            *string = escape(string, control)?;
        }
        JsonValue::Array(values) => {
            for value in values {
                encode(value, control)?;
            }
        }
        JsonValue::Object(values) => {
            for value in values.values_mut() {
                encode(value, control)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn escape(string: &str, control: Option<&ExecutionControl<'_>>) -> QueryResult<String> {
    let capacity = TEXT.len().checked_add(string.len()).ok_or_else(|| {
        super::super::super::resources::memory_error("spill string capacity overflow")
    })?;
    check(control)?;
    let mut escaped = String::with_capacity(capacity);
    escaped.push_str(TEXT);
    crate::query::functions::append_text_controlled(&mut escaped, string, control)?;
    Ok(escaped)
}

pub(super) fn decode(value: &mut JsonValue, control: &ExecutionControl<'_>) -> QueryResult<()> {
    control.work(1)?;
    match value {
        JsonValue::String(string) => {
            if let Some(original) = string.strip_prefix(TEXT) {
                *string = text(original, Some(control))?;
            } else if let Some(bits) = string.strip_prefix(FLOAT) {
                *value = restore(bits)?;
            }
        }
        JsonValue::Array(values) => {
            for value in values {
                decode(value, control)?;
            }
        }
        JsonValue::Object(values) => {
            for value in values.values_mut() {
                decode(value, control)?;
            }
        }
        _ => {}
    }
    Ok(())
}

fn restore(encoded: &str) -> QueryResult<JsonValue> {
    let bits = (encoded.len() == 16)
        .then(|| u64::from_str_radix(encoded, 16).ok())
        .flatten()
        .ok_or_else(|| QueryError::internal("invalid finite spill Float bits"))?;
    let number = serde_json::Number::from_f64(f64::from_bits(bits))
        .ok_or_else(|| QueryError::internal("non-finite spill Float bits"))?;
    Ok(JsonValue::Number(number))
}
