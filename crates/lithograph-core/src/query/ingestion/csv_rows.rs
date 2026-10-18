use std::io::{BufRead, BufReader, Read};

use crate::query::resources::{ExecutionControl, MemoryReservation};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

use super::buffers::{reserve_string, reserve_vector};

const READER_BYTES: usize = 8 * 1024;

pub(super) struct CsvRows {
    reader: BufReader<Box<dyn Read>>,
    delimiter: char,
    first_line: bool,
    _reader_memory: MemoryReservation,
}

pub(super) struct CsvRecord {
    pub(super) fields: Vec<String>,
    pub(super) memory: MemoryReservation,
}

impl CsvRows {
    pub(super) fn new(
        reader: Box<dyn Read>,
        delimiter: char,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let memory = control.reserve(READER_BYTES as u64)?;
        Ok(Self {
            reader: BufReader::with_capacity(READER_BYTES, reader),
            delimiter,
            first_line: true,
            _reader_memory: memory,
        })
    }

    pub(super) fn next_record(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<CsvRecord>> {
        control.check()?;
        let mut state = CsvRecordState::new(control)?;
        loop {
            let Some((line, _line_memory)) = self.read_line(control)? else {
                return state.finish_eof(control);
            };
            self.consume_line(&line, &mut state, control)?;
            if state.complete {
                return Ok(Some(state.into_record()));
            }
        }
    }

    fn read_line(
        &mut self,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<(String, MemoryReservation)>> {
        let mut memory = control.reserve(0)?;
        let mut line = Vec::new();
        loop {
            control.check()?;
            let bytes = self
                .reader
                .fill_buf()
                .map_err(|error| load_csv_io_error(format!("LOAD CSV read failed: {error}")))?;
            control.check()?;
            if bytes.is_empty() {
                break;
            }
            let ending = bytes.iter().position(|byte| *byte == b'\n');
            let count = ending.map_or(bytes.len(), |index| index + 1);
            control.bytes(count as u64)?;
            reserve_vector(&mut line, count, &mut memory, control)?;
            line.extend_from_slice(&bytes[..count]);
            self.reader.consume(count);
            if ending.is_some() {
                break;
            }
        }
        if line.is_empty() {
            return Ok(None);
        }
        let mut line = String::from_utf8(line)
            .map_err(|error| load_csv_io_error(format!("LOAD CSV read failed: {error}")))?;
        if self.first_line && line.starts_with('\u{feff}') {
            drop(line.drain(..3));
        }
        self.first_line = false;
        Ok(Some((line, memory)))
    }

    fn consume_line(
        &self,
        line: &str,
        state: &mut CsvRecordState,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        state.saw_input = true;
        let mut chars = line.chars().peekable();
        while let Some(character) = chars.next() {
            control.work(1)?;
            if state.in_quotes {
                consume_quoted_character(character, &mut chars, state, control)?;
            } else {
                consume_outside_quotes(character, &mut chars, self.delimiter, state, control)?;
            }
            if state.complete {
                break;
            }
        }
        Ok(())
    }
}

struct CsvRecordState {
    fields: Vec<String>,
    field: String,
    in_quotes: bool,
    after_quote: bool,
    saw_input: bool,
    complete: bool,
    memory: MemoryReservation,
}

impl CsvRecordState {
    fn new(control: &ExecutionControl<'_>) -> QueryResult<Self> {
        Ok(Self {
            fields: Vec::new(),
            field: String::new(),
            in_quotes: false,
            after_quote: false,
            saw_input: false,
            complete: false,
            memory: control.reserve(0)?,
        })
    }

    fn push_character(&mut self, value: char, control: &ExecutionControl<'_>) -> QueryResult<()> {
        reserve_string(&mut self.field, value.len_utf8(), &mut self.memory, control)?;
        self.field.push(value);
        Ok(())
    }

    fn finish_field(&mut self, control: &ExecutionControl<'_>) -> QueryResult<()> {
        reserve_vector(&mut self.fields, 1, &mut self.memory, control)?;
        self.fields.push(std::mem::take(&mut self.field));
        Ok(())
    }

    fn finish_eof(mut self, control: &ExecutionControl<'_>) -> QueryResult<Option<CsvRecord>> {
        if self.in_quotes {
            return Err(load_csv_error("LOAD CSV ended inside a quoted field"));
        }
        if !self.saw_input && self.field.is_empty() && self.fields.is_empty() {
            return Ok(None);
        }
        self.finish_field(control)?;
        Ok(Some(self.into_record()))
    }

    fn into_record(self) -> CsvRecord {
        CsvRecord {
            fields: self.fields,
            memory: self.memory,
        }
    }
}

fn consume_quoted_character(
    character: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    state: &mut CsvRecordState,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    if character == '\\' && chars.peek() == Some(&'"') {
        chars.next();
        return state.push_character('"', control);
    }
    if character != '"' {
        return state.push_character(character, control);
    }
    if chars.peek() == Some(&'"') {
        chars.next();
        state.push_character('"', control)?;
    } else {
        state.in_quotes = false;
        state.after_quote = true;
    }
    Ok(())
}

fn consume_outside_quotes(
    character: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    delimiter: char,
    state: &mut CsvRecordState,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    if consume_delimiter_or_record_ending(character, chars, delimiter, state, control)? {
        state.after_quote = false;
        return Ok(());
    }
    if state.after_quote {
        return Err(load_csv_error(
            "LOAD CSV has characters after a closing quote",
        ));
    }
    if character == '"' {
        if !state.field.is_empty() {
            return Err(load_csv_error(
                "LOAD CSV has an unexpected quote in an unquoted field",
            ));
        }
        state.in_quotes = true;
    } else {
        state.push_character(character, control)?;
    }
    Ok(())
}

fn consume_delimiter_or_record_ending(
    character: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    delimiter: char,
    state: &mut CsvRecordState,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if character == delimiter {
        state.finish_field(control)?;
        return Ok(true);
    }
    if !matches!(character, '\r' | '\n') {
        return Ok(false);
    }
    if character == '\r' && chars.peek() == Some(&'\n') {
        chars.next();
    }
    state.finish_field(control)?;
    state.complete = true;
    Ok(true)
}

fn load_csv_error(message: impl Into<String>) -> QueryError {
    QueryError::new(QueryErrorKind::Resource, message)
}

fn load_csv_io_error(message: impl Into<String>) -> QueryError {
    QueryError::io(message)
}

#[cfg(test)]
mod tests;
