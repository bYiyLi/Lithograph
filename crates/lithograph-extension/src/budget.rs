//! Adapter allocations share the execution budget, including temporary JSON
//! trees and the serialized payload that exists alongside them.

use super::*;
use std::collections::BTreeMap;
use std::io::{self, Write};
use std::mem::size_of;

pub(super) struct BudgetedJson {
    pub value: Value,
    pub reservation: query::MemoryReservation,
}

pub(super) struct BudgetedString {
    pub text: String,
    pub _reservation: query::MemoryReservation,
}

#[derive(Debug)]
enum ParameterDecodeError {
    Value(cypher::ValueError),
    Execution(query::QueryError),
}

impl From<cypher::ValueError> for ParameterDecodeError {
    fn from(error: cypher::ValueError) -> Self {
        Self::Value(error)
    }
}

impl ParameterDecodeError {
    fn into_adapter(self) -> LithographError {
        match self {
            Self::Value(error) => LithographError::invalid_argument(error.message),
            Self::Execution(error) => execution::map_query_error(error),
        }
    }
}

fn reserve(
    resources: &query::QueryResources,
    bytes: u64,
) -> LithographResult<query::MemoryReservation> {
    resources.reserve(bytes).map_err(execution::map_query_error)
}

fn checked_add(left: u64, right: u64) -> LithographResult<u64> {
    query::resources::checked_add(left, right).map_err(execution::map_query_error)
}

fn checked_mul(left: u64, right: u64) -> LithographResult<u64> {
    query::resources::checked_mul(left, right).map_err(execution::map_query_error)
}

pub(super) fn copy_text(
    text: &str,
    control: &query::ExecutionControl<'_>,
) -> LithographResult<String> {
    control.check().map_err(execution::map_query_error)?;
    let mut result = String::with_capacity(text.len());
    let mut offset = 0;
    while offset < text.len() {
        let mut end = offset.saturating_add(64 * 1024).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        control
            .bytes((end - offset) as u64)
            .map_err(execution::map_query_error)?;
        result.push_str(&text[offset..end]);
        offset = end;
    }
    Ok(result)
}

/// Inspect lexical tokens without building a JSON tree. Each token reserves
/// enough for both serde's temporary tree and the decoded runtime value, with
/// geometric Vec capacity and BTree node overhead. String bytes are reserved
/// for both representations; JSON escapes cannot decode to a larger string.
fn parameter_preflight_bytes(
    text: &str,
    control: &query::ExecutionControl<'_>,
) -> LithographResult<u64> {
    let bytes = text.as_bytes();
    let mut index = 0;
    let mut required = 0;
    // Growing a Vec can briefly retain its old and new buffers together.
    let token_bytes = (3 * size_of::<cypher::Value>() + 3 * size_of::<Value>()) as u64;
    let map_entry_bytes = checked_add(
        query::resources::map_entry_bytes::<cypher::Value>().map_err(execution::map_query_error)?,
        query::resources::map_entry_bytes::<Value>().map_err(execution::map_query_error)?,
    )?;
    while index < bytes.len() {
        control.bytes(1).map_err(execution::map_query_error)?;
        match bytes[index] {
            b'"' => {
                let start = index;
                index = quoted_token_end(bytes, index, control)?;
                required = checked_add(required, token_bytes)?;
                required = checked_add(required, checked_mul((index - start) as u64, 4)?)?;
                // Conservatively allow a sparse BTree entry for each string
                // token without constructing a second structural parse tree.
                required = checked_add(required, map_entry_bytes)?;
            }
            b'{' | b'[' => {
                required = checked_add(required, token_bytes)?;
                index += 1;
            }
            b'}' | b']' | b',' | b':' | b' ' | b'\t' | b'\n' | b'\r' => index += 1,
            _ => {
                required = checked_add(required, token_bytes)?;
                index = primitive_token_end(bytes, index, control)?;
            }
        }
    }
    Ok(required)
}

fn quoted_token_end(
    bytes: &[u8],
    start: usize,
    control: &query::ExecutionControl<'_>,
) -> LithographResult<usize> {
    let mut index = start + 1;
    while index < bytes.len() {
        let current = bytes[index];
        let step = if current == b'\\' { 2 } else { 1 };
        control
            .bytes(step as u64)
            .map_err(execution::map_query_error)?;
        index = index.saturating_add(step);
        if current == b'"' {
            break;
        }
    }
    Ok(index)
}

fn primitive_token_end(
    bytes: &[u8],
    start: usize,
    control: &query::ExecutionControl<'_>,
) -> LithographResult<usize> {
    let mut index = start + 1;
    while index < bytes.len()
        && !matches!(
            bytes[index],
            b'}' | b']' | b',' | b':' | b' ' | b'\t' | b'\n' | b'\r'
        )
    {
        control.bytes(1).map_err(execution::map_query_error)?;
        index += 1;
    }
    Ok(index)
}

pub(super) fn decode_parameters(
    text: &str,
    resources: &query::QueryResources,
    is_interrupted: &dyn Fn() -> bool,
) -> LithographResult<(BTreeMap<String, cypher::Value>, query::MemoryReservation)> {
    let control = query::ExecutionControl::new(resources, is_interrupted);
    control.check().map_err(execution::map_query_error)?;
    let preflight = parameter_preflight_bytes(text, &control)?;
    let reservation = reserve(resources, preflight)?;
    control.check().map_err(execution::map_query_error)?;
    let raw: Value = serde_json::from_str(text).map_err(|error| {
        LithographError::invalid_argument(format!("invalid parameters JSON: {error}"))
    })?;
    let parameters = cypher::decode_parameters_checked(&raw, &mut |units| {
        if units == 0 {
            control.check()
        } else {
            control.work(units as u64)
        }
        .map_err(ParameterDecodeError::Execution)
    })
    .map_err(ParameterDecodeError::into_adapter)?;
    control.check().map_err(execution::map_query_error)?;
    drop(raw);
    Ok((parameters, reservation))
}

pub(super) fn row_json(
    row: &[cypher::Value],
    access: &query::AccessPolicy,
    resources: &query::QueryResources,
    is_interrupted: &dyn Fn() -> bool,
) -> LithographResult<BudgetedJson> {
    let control = query::ExecutionControl::new(resources, is_interrupted);
    control.check().map_err(execution::map_query_error)?;
    // The encoder moves owned children into their parent and filters graph
    // properties while encoding, avoiding a second cloned runtime graph value.
    let footprint = query::resources::encoded_row_bytes_controlled(row, &control)
        .map_err(execution::map_query_error)?;
    let mut reservation = reserve(resources, footprint)?;
    control.check().map_err(execution::map_query_error)?;
    let mut checkpoint = |work, bytes| {
        if work == 0 && bytes == 0 {
            control.check()
        } else {
            control.work(work).and_then(|()| control.bytes(bytes))
        }
    };
    let allow_property =
        |name: &str| !access.read.query.denies(name) && !access.read.result.denies(name);
    let value = Value::Array(
        row.iter()
            .map(|value| {
                cypher::encode_json_filtered_checked(value, &mut checkpoint, &allow_property)
            })
            .collect::<query::QueryResult<_>>()
            .map_err(execution::map_query_error)?,
    );
    reservation
        .resize(
            query::resources::json_bytes_controlled(&value, &control)
                .map_err(execution::map_query_error)?,
        )
        .map_err(execution::map_query_error)?;
    Ok(BudgetedJson { value, reservation })
}

pub(super) fn json_owned(
    resources: &query::QueryResources,
    preflight: u64,
    build: impl FnOnce() -> Value,
) -> LithographResult<BudgetedJson> {
    let mut reservation = reserve(resources, preflight)?;
    let value = build();
    reservation
        .resize(query::resources::json_bytes(&value).map_err(execution::map_query_error)?)
        .map_err(execution::map_query_error)?;
    Ok(BudgetedJson { value, reservation })
}

struct JsonLength<'a> {
    bytes: u64,
    control: query::ExecutionControl<'a>,
    interrupted: bool,
}

impl Write for JsonLength<'_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        check_serialization_work(&self.control, &mut self.interrupted, bytes.len())?;
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| io::Error::other("JSON length overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

pub(super) fn serialize_json(
    value: &Value,
    resources: &query::QueryResources,
    is_interrupted: &dyn Fn() -> bool,
) -> LithographResult<BudgetedString> {
    let control = query::ExecutionControl::new(resources, is_interrupted);
    control.check().map_err(execution::map_query_error)?;
    let mut length = JsonLength {
        bytes: 0,
        control,
        interrupted: false,
    };
    serde_json::to_writer(&mut length, value)
        .map_err(|error| json_output_error(error, length.interrupted, "measure"))?;
    let reservation = reserve(resources, length.bytes)?;
    let capacity = usize::try_from(length.bytes).map_err(|_| {
        LithographError::new(
            ErrorCategory::Resource,
            "JSON output exceeds addressable memory",
            ffi::SQLITE_TOOBIG,
        )
    })?;
    let mut output = Vec::with_capacity(capacity);
    let mut writer = InterruptWriter {
        output: &mut output,
        control: &length.control,
        interrupted: false,
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|error| json_output_error(error, writer.interrupted, "encode"))?;
    length.control.check().map_err(execution::map_query_error)?;
    let text = String::from_utf8(output)
        .map_err(|error| LithographError::internal(format!("JSON output is not UTF-8: {error}")))?;
    Ok(BudgetedString {
        text,
        _reservation: reservation,
    })
}

struct InterruptWriter<'a, 'b> {
    output: &'a mut Vec<u8>,
    control: &'a query::ExecutionControl<'b>,
    interrupted: bool,
}

fn check_serialization_work(
    control: &query::ExecutionControl<'_>,
    interrupted: &mut bool,
    bytes: usize,
) -> io::Result<()> {
    control
        .bytes(bytes as u64)
        .and_then(|()| control.work(1))
        .map_err(|_| {
            *interrupted = true;
            io::Error::other("query interrupted")
        })
}

fn json_output_error(error: serde_json::Error, interrupted: bool, action: &str) -> LithographError {
    if interrupted {
        execution::map_query_error(query::QueryError::interrupted())
    } else {
        LithographError::internal(format!("failed to {action} JSON output: {error}"))
    }
}

impl Write for InterruptWriter<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        for chunk in bytes.chunks(64 * 1024) {
            check_serialization_work(self.control, &mut self.interrupted, chunk.len())?;
            self.output.extend_from_slice(chunk);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parameters_are_rejected_before_large_decode_and_release_on_error() {
        let resources = query::QueryResources::new(Some(4096));
        let text = format!("{{\"value\":[{}]}}", "1,".repeat(10000));
        let error = decode_parameters(&text, &resources, &|| false).expect_err("budget");
        assert_eq!(error.category, ErrorCategory::Resource);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn json_tree_and_serialized_copy_overlap_then_release() {
        let resources = query::QueryResources::new(Some(16384));
        let tree =
            json_owned(&resources, 4096, || json!(["quoted\"text", "line\nend"])).expect("tree");
        let used = resources.used();
        let output = serialize_json(&tree.value, &resources, &|| false).expect("encoded");
        assert_eq!(resources.used(), used + output.text.len() as u64);
        assert_eq!(
            serde_json::from_str::<Value>(&output.text).expect("JSON"),
            tree.value
        );
        drop(tree);
        assert_eq!(resources.used(), output.text.len() as u64);
        drop(output);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn checked_encoder_interrupts_value_walk_and_releases_tree_reservation() {
        let resources = query::QueryResources::default();
        let row = vec![cypher::Value::List(
            (0..10000).map(cypher::Value::Integer).collect(),
        )];
        let checks = std::cell::Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() > 3
        };
        let error = match row_json(
            &row,
            &query::AccessPolicy::default(),
            &resources,
            &interrupted,
        ) {
            Ok(_) => panic!("expected interrupt"),
            Err(error) => error,
        };
        assert_eq!(error.category, ErrorCategory::Interrupted);
        assert_eq!(error.sqlite_code, ffi::SQLITE_INTERRUPT);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn serialization_interrupt_releases_output_reservation() {
        let resources = query::QueryResources::default();
        let value = json!(["x".repeat(512 * 1024)]);
        let checks = std::cell::Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() > 4
        };
        let error = match serialize_json(&value, &resources, &interrupted) {
            Ok(_) => panic!("expected interrupt"),
            Err(error) => error,
        };
        assert_eq!(error.category, ErrorCategory::Interrupted);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn nested_typed_duration_uses_the_host_checkpoint_during_parameter_decode() {
        let resources = query::QueryResources::default();
        let duration = format!("P{}1D", "0".repeat(10000));
        let input = json!({"nested": {"$type": "Map", "entries": {
            "values": [{"$type": "Duration", "value": duration}]
        }}})
        .to_string();
        let checks = std::cell::Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() >= 20
        };
        let error =
            decode_parameters(&input, &resources, &interrupted).expect_err("Duration cancellation");
        assert_eq!(error.category, ErrorCategory::Interrupted);
        assert_eq!(error.sqlite_code, ffi::SQLITE_INTERRUPT);
        assert_eq!(checks.get(), 20);
        assert_eq!(resources.used(), 0);

        let (decoded, reservation) =
            decode_parameters(&input, &resources, &|| false).expect("valid Duration");
        assert!(decoded.contains_key("nested"));
        drop(decoded);
        drop(reservation);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn malformed_typed_duration_remains_invalid_argument() {
        let resources = query::QueryResources::default();
        let error = decode_parameters(
            r#"{"d":{"$type":"Duration","value":"invalid"}}"#,
            &resources,
            &|| false,
        )
        .expect_err("malformed Duration");
        assert_eq!(error.category, ErrorCategory::InvalidArgument);
        assert_ne!(error.sqlite_code, ffi::SQLITE_INTERRUPT);
        assert_eq!(resources.used(), 0);
    }
}
