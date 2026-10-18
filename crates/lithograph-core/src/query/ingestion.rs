use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::cypher::{AstKind, AstNode, Value};

use super::expression::{
    self, BindingRow, BindingValue, LoadCsvContext, compile_expression, surface_expressions,
};
use super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, checked_mul,
    map_entry_bytes, value_bytes_controlled,
};
use super::{QueryError, QueryErrorKind, QueryResult};

mod binding;
mod buffers;
mod csv_rows;
pub(crate) use binding::csv_binding_row;

use csv_rows::CsvRows;

pub(crate) struct CompiledLoadCsv {
    pub(crate) binding: String,
    pub(crate) source_expression: expression::Expr,
    pub(crate) delimiter_expression: Option<expression::Expr>,
    pub(crate) with_headers: bool,
}

pub(crate) fn compile_load_csv(clause: &AstNode) -> QueryResult<CompiledLoadCsv> {
    let binding = clause
        .descendants()
        .find(|node| node.kind == AstKind::LoadCsvBinding)
        .and_then(|node| node.text.as_deref())
        .map(crate::cypher::unescape_identifier)
        .ok_or_else(|| QueryError::semantic("LOAD CSV is missing its AS binding"))?;
    let source = surface_expressions(clause)
        .into_iter()
        .next()
        .ok_or_else(|| QueryError::semantic("LOAD CSV is missing its source expression"))?;
    let delimiter_expression = clause
        .descendants()
        .find(|node| node.kind == AstKind::LoadCsvFieldTerminator)
        .and_then(|node| {
            node.descendants()
                .find(|child| matches!(child.kind, AstKind::Literal(_)))
        })
        .map(compile_expression)
        .transpose()?;
    Ok(CompiledLoadCsv {
        binding,
        source_expression: compile_expression(source)?,
        delimiter_expression,
        with_headers: clause
            .descendants()
            .any(|node| node.kind == AstKind::LoadCsvHeaders),
    })
}

pub(crate) fn require_csv_string(value: Value, role: &str) -> QueryResult<String> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            format!("LOAD CSV {role} must evaluate to String"),
        )),
    }
}

pub(crate) fn csv_delimiter(value: Value) -> QueryResult<char> {
    let value = require_csv_string(value, "FIELDTERMINATOR")?;
    let mut characters = value.chars();
    let Some(delimiter) = characters.next() else {
        return Err(QueryError::invalid_argument(
            "LOAD CSV FIELDTERMINATOR must contain exactly one character",
        ));
    };
    if characters.next().is_some() || matches!(delimiter, '\r' | '\n' | '"') {
        return Err(QueryError::invalid_argument(
            "LOAD CSV FIELDTERMINATOR must contain one non-quote, non-newline character",
        ));
    }
    Ok(delimiter)
}

pub(crate) fn evaluate_csv_source(
    spec: &CompiledLoadCsv,
    mut evaluate: impl FnMut(&expression::Expr) -> QueryResult<Value>,
) -> QueryResult<(String, char)> {
    let source = require_csv_string(evaluate(&spec.source_expression)?, "source")?;
    let delimiter = spec
        .delimiter_expression
        .as_ref()
        .map(&mut evaluate)
        .transpose()?
        .map_or(Ok(','), csv_delimiter)?;
    Ok((source, delimiter))
}

pub(crate) fn stream_csv_binding_rows(
    spec: &CompiledLoadCsv,
    input: &BindingRow,
    source: &str,
    delimiter: char,
    control: &ExecutionControl<'_>,
    mut emit: impl FnMut(BindingRow, MemoryReservation) -> QueryResult<()>,
) -> QueryResult<Option<tempfile::TempPath>> {
    let mut stream = CsvStream::open(source, delimiter, spec.with_headers, control)?;
    while let Some((value, line, memory)) = stream.next_value(control)? {
        let (row, memory) = csv_binding_row(
            input,
            &spec.binding,
            value,
            stream.file_path(),
            line,
            memory,
            control,
        )?;
        emit(row, memory)?;
    }
    Ok(stream.into_source_guard())
}

pub(crate) struct CsvStream {
    file_path: String,
    rows: CsvRows,
    temporary_path: Option<tempfile::TempPath>,
    headers: Option<Vec<String>>,
    with_headers: bool,
    logical_row: u64,
    _source_memory: MemoryReservation,
    headers_memory: Option<MemoryReservation>,
}

impl CsvStream {
    pub(crate) fn open(
        source: &str,
        delimiter: char,
        with_headers: bool,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        // OS path resolution and URL decoding may retain multiple copies. This
        // allowance precedes those allocations; native/HTTP calls are checked
        // before and after their existing blocking I/O boundary.
        let mut memory = control.reserve(checked_add(
            checked_mul(source.len() as u64, 8)?,
            64 * 1024,
        )?)?;
        let opened = open_source(source, control)?;
        control.check()?;
        memory.resize(opened.file_path.capacity() as u64)?;
        Ok(Self {
            file_path: opened.file_path,
            rows: CsvRows::new(opened.reader, delimiter, control)?,
            temporary_path: opened.temporary_path,
            headers: None,
            with_headers,
            logical_row: 0,
            _source_memory: memory,
            headers_memory: None,
        })
    }

    pub(crate) fn next_value(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(Value, u64, MemoryReservation)>> {
        control.check()?;
        if self.with_headers && self.headers.is_none() {
            let Some(headers) = self.rows.next_record(control)? else {
                return Ok(None);
            };
            validate_headers(&headers.fields, control)?;
            self.headers = Some(headers.fields);
            self.headers_memory = Some(headers.memory);
            self.logical_row = 1;
        }
        let Some(record) = self.rows.next_record(control)? else {
            return Ok(None);
        };
        self.logical_row = self.logical_row.saturating_add(1);
        let (value, memory) = record_value(record, self.headers.as_deref(), control)?;
        Ok(Some((value, self.logical_row, memory)))
    }

    pub(crate) fn file_path(&self) -> &str {
        &self.file_path
    }

    pub(crate) fn into_source_guard(self) -> Option<tempfile::TempPath> {
        self.temporary_path
    }
}

fn validate_headers(headers: &[String], control: &ExecutionControl<'_>) -> QueryResult<()> {
    let _memory = control.reserve(checked_mul(headers.len() as u64, 128)?)?;
    let mut unique = BTreeSet::new();
    for header in headers {
        control.work(1)?;
        if header.is_empty() {
            return Err(load_csv_error("LOAD CSV header names cannot be empty"));
        }
        if !unique.insert(header) {
            return Err(load_csv_error(format!(
                "LOAD CSV header {header:?} is duplicated"
            )));
        }
    }
    Ok(())
}

fn record_value(
    record: csv_rows::CsvRecord,
    headers: Option<&[String]>,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Value, MemoryReservation)> {
    let mut memory = record.memory;
    let value = if let Some(headers) = headers {
        record_map(record.fields, headers, &mut memory, control)?
    } else {
        let output_memory = control.reserve(allocation_bytes::<Value>(record.fields.len())?)?;
        let mut values = Vec::with_capacity(record.fields.len());
        for field in record.fields {
            control.work(1)?;
            values.push(Value::String(field));
        }
        memory.merge(output_memory)?;
        Value::List(values)
    };
    memory.resize(value_bytes_controlled(&value, control)?)?;
    Ok((value, memory))
}

fn record_map(
    fields: Vec<String>,
    headers: &[String],
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<Value> {
    let mut bytes = checked_mul(headers.len() as u64, map_entry_bytes::<Value>()?)?;
    for header in headers {
        bytes = checked_add(bytes, header.len() as u64)?;
        control.work(1)?;
    }
    let map_memory = control.reserve(bytes)?;
    let mut fields = fields.into_iter();
    let mut values = BTreeMap::new();
    for header in headers {
        control.work(1)?;
        values.insert(
            expression::clone_string_controlled(header, Some(control))?,
            fields.next().map_or(Value::Null, Value::String),
        );
    }
    memory.merge(map_memory)?;
    Ok(Value::Map(values))
}

struct OpenedCsvSource {
    reader: Box<dyn Read>,
    file_path: String,
    temporary_path: Option<tempfile::TempPath>,
}

struct RemoteCsvReader<R> {
    reader: R,
    temporary: File,
}

impl<R: Read> Read for RemoteCsvReader<R> {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        let read = self.reader.read(buffer)?;
        if read > 0 {
            self.temporary.write_all(&buffer[..read])?;
        }
        Ok(read)
    }
}

fn open_source(source: &str, control: &ExecutionControl<'_>) -> QueryResult<OpenedCsvSource> {
    control.check()?;
    if let Some(path) = source.strip_prefix("file://") {
        let path = decode_file_url(path, control)?;
        let absolute = fs::canonicalize(&path).map_err(|error| {
            load_csv_io_error(format!(
                "LOAD CSV failed to open {}: {error}",
                path.display()
            ))
        })?;
        control.check()?;
        let file = File::open(&absolute).map_err(|error| {
            load_csv_io_error(format!(
                "LOAD CSV failed to open {}: {error}",
                absolute.display()
            ))
        })?;
        control.check()?;
        return Ok(OpenedCsvSource {
            reader: Box::new(file),
            file_path: absolute.to_string_lossy().into_owned(),
            temporary_path: None,
        });
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = ureq::get(source)
            .call()
            .map_err(|_| load_csv_io_error("LOAD CSV request failed"))?;
        control.check()?;
        let temporary = tempfile::NamedTempFile::new().map_err(|error| {
            load_csv_io_error(format!("LOAD CSV failed to create temporary file: {error}"))
        })?;
        let file_path = temporary.path().to_string_lossy().into_owned();
        let writer = temporary.reopen().map_err(|error| {
            load_csv_io_error(format!("LOAD CSV failed to reopen temporary file: {error}"))
        })?;
        let temporary_path = temporary.into_temp_path();
        return Ok(OpenedCsvSource {
            reader: Box::new(RemoteCsvReader {
                reader: response.into_body().into_reader(),
                temporary: writer,
            }),
            file_path,
            temporary_path: Some(temporary_path),
        });
    }
    Err(QueryError::invalid_argument(
        "LOAD CSV source must use file://, http://, or https://",
    ))
}

fn decode_file_url(value: &str, control: &ExecutionControl<'_>) -> QueryResult<PathBuf> {
    let decoded = if value.starts_with('/') {
        percent_decode_path(value, control)?
    } else {
        let (host, path) = value.split_once('/').unwrap_or((value, ""));
        if !host.is_empty() && host != "localhost" {
            return Err(QueryError::invalid_argument(
                "LOAD CSV file:// supports only local files",
            ));
        }
        percent_decode_path(&format!("/{path}"), control)?
    };
    Ok(local_file_url_path(decoded))
}

#[cfg(windows)]
fn local_file_url_path(value: String) -> PathBuf {
    windows_drive_file_url_path(&value)
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from(value))
}

#[cfg(not(windows))]
fn local_file_url_path(value: String) -> PathBuf {
    PathBuf::from(value)
}

#[cfg(any(windows, test))]
fn windows_drive_file_url_path(value: &str) -> Option<&str> {
    let path = value.strip_prefix('/')?;
    let bytes = path.as_bytes();
    (bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':').then_some(path)
}

fn percent_decode_path(value: &str, control: &ExecutionControl<'_>) -> QueryResult<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
        control.work(1)?;
        if bytes[index] == b'%' {
            if index + 2 >= bytes.len() {
                return Err(QueryError::invalid_argument(
                    "LOAD CSV file URL has invalid percent encoding",
                ));
            }
            let high = hex_value(bytes[index + 1])?;
            let low = hex_value(bytes[index + 2])?;
            decoded.push((high << 4) | low);
            index += 3;
        } else {
            decoded.push(bytes[index]);
            index += 1;
        }
    }
    String::from_utf8(decoded)
        .map_err(|_| QueryError::invalid_argument("LOAD CSV file URL path is not valid UTF-8"))
}

fn hex_value(value: u8) -> QueryResult<u8> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        b'A'..=b'F' => Ok(value - b'A' + 10),
        _ => Err(QueryError::invalid_argument(
            "LOAD CSV file URL has invalid percent encoding",
        )),
    }
}

fn load_csv_io_error(message: impl Into<String>) -> QueryError {
    QueryError::io(message)
}

fn load_csv_error(message: impl Into<String>) -> QueryError {
    QueryError::new(QueryErrorKind::Resource, message)
}

#[cfg(test)]
mod tests;
