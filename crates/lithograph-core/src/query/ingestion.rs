use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, File};
use std::io::{Read, Write};
use std::path::PathBuf;

use crate::cypher::{AstKind, AstNode, Value};

use super::expression::{self, BindingRow, BindingValue, compile_expression, surface_expressions};
use super::{QueryError, QueryErrorKind, QueryResult};

mod csv_rows;

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

pub(crate) fn csv_binding_row(
    input: &BindingRow,
    binding: &str,
    value: Value,
    source: &str,
    line: u64,
) -> BindingRow {
    let mut row = input.clone();
    row.insert(binding.to_owned(), BindingValue::Scalar(value));
    row.values.insert(
        "__lithograph_load_csv_file".to_owned(),
        BindingValue::Scalar(Value::String(source.to_owned())),
    );
    row.values.insert(
        "__lithograph_load_csv_line".to_owned(),
        BindingValue::Scalar(Value::Integer(i64::try_from(line).unwrap_or(i64::MAX))),
    );
    row
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

pub(crate) struct CsvStream {
    file_path: String,
    rows: CsvRows,
    headers: Option<Vec<String>>,
    with_headers: bool,
    logical_row: u64,
}

impl CsvStream {
    pub(crate) fn open(source: &str, delimiter: char, with_headers: bool) -> QueryResult<Self> {
        let opened = open_source(source)?;
        Ok(Self {
            file_path: opened.file_path,
            rows: CsvRows::new(opened.reader, delimiter),
            headers: None,
            with_headers,
            logical_row: 0,
        })
    }

    pub(crate) fn next_value(&mut self) -> QueryResult<Option<(Value, u64)>> {
        if self.with_headers && self.headers.is_none() {
            let Some(headers) = self.rows.next_record()? else {
                return Ok(None);
            };
            validate_headers(&headers)?;
            self.headers = Some(headers);
            self.logical_row = 1;
        }
        let Some(fields) = self.rows.next_record()? else {
            return Ok(None);
        };
        self.logical_row = self.logical_row.saturating_add(1);
        let value = if let Some(headers) = &self.headers {
            Value::Map(
                headers
                    .iter()
                    .enumerate()
                    .map(|(index, header)| {
                        let value = fields
                            .get(index)
                            .cloned()
                            .map(Value::String)
                            .unwrap_or(Value::Null);
                        (header.clone(), value)
                    })
                    .collect::<BTreeMap<_, _>>(),
            )
        } else {
            Value::List(fields.into_iter().map(Value::String).collect())
        };
        Ok(Some((value, self.logical_row)))
    }

    pub(crate) fn file_path(&self) -> &str {
        &self.file_path
    }
}

fn validate_headers(headers: &[String]) -> QueryResult<()> {
    let mut unique = BTreeSet::new();
    for header in headers {
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

struct OpenedCsvSource {
    reader: Box<dyn Read>,
    file_path: String,
}

struct RemoteCsvReader<R> {
    reader: R,
    temporary: tempfile::NamedTempFile,
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

fn open_source(source: &str) -> QueryResult<OpenedCsvSource> {
    if let Some(path) = source.strip_prefix("file://") {
        let path = decode_file_url(path)?;
        let absolute = fs::canonicalize(&path).map_err(|error| {
            load_csv_error(format!(
                "LOAD CSV failed to open {}: {error}",
                path.display()
            ))
        })?;
        let file = File::open(&absolute).map_err(|error| {
            load_csv_error(format!(
                "LOAD CSV failed to open {}: {error}",
                absolute.display()
            ))
        })?;
        return Ok(OpenedCsvSource {
            reader: Box::new(file),
            file_path: absolute.to_string_lossy().into_owned(),
        });
    }
    if source.starts_with("http://") || source.starts_with("https://") {
        let response = ureq::get(source).call().map_err(|error| {
            load_csv_error(format!("LOAD CSV request failed for {source}: {error}"))
        })?;
        let temporary = tempfile::NamedTempFile::new().map_err(|error| {
            load_csv_error(format!("LOAD CSV failed to create temporary file: {error}"))
        })?;
        let file_path = temporary.path().to_string_lossy().into_owned();
        return Ok(OpenedCsvSource {
            reader: Box::new(RemoteCsvReader {
                reader: response.into_body().into_reader(),
                temporary,
            }),
            file_path,
        });
    }
    Err(QueryError::invalid_argument(
        "LOAD CSV source must use file://, http://, or https://",
    ))
}

fn decode_file_url(value: &str) -> QueryResult<PathBuf> {
    if value.starts_with('/') {
        return percent_decode_path(value).map(PathBuf::from);
    }
    let (host, path) = value.split_once('/').unwrap_or((value, ""));
    if !host.is_empty() && host != "localhost" {
        return Err(QueryError::invalid_argument(
            "LOAD CSV file:// supports only local files",
        ));
    }
    percent_decode_path(&format!("/{path}")).map(PathBuf::from)
}

fn percent_decode_path(value: &str) -> QueryResult<String> {
    let bytes = value.as_bytes();
    let mut decoded = Vec::with_capacity(bytes.len());
    let mut index = 0;
    while index < bytes.len() {
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

fn load_csv_error(message: impl Into<String>) -> QueryError {
    QueryError::new(QueryErrorKind::Resource, message)
}
