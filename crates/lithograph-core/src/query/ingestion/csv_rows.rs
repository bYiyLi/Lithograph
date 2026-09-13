use std::io::{BufRead, BufReader, Read};

use crate::query::{QueryError, QueryErrorKind, QueryResult};

pub(super) struct CsvRows {
    reader: BufReader<Box<dyn Read>>,
    delimiter: char,
    first_record: bool,
}

impl CsvRows {
    pub(super) fn new(reader: Box<dyn Read>, delimiter: char) -> Self {
        Self {
            reader: BufReader::new(reader),
            delimiter,
            first_record: true,
        }
    }

    pub(super) fn next_record(&mut self) -> QueryResult<Option<Vec<String>>> {
        let mut state = CsvRecordState::default();
        loop {
            let Some(line) = self.read_line()? else {
                return state.finish_eof();
            };
            self.consume_line(&line, &mut state)?;
            if state.complete {
                self.first_record = false;
                return Ok(Some(state.fields));
            }
        }
    }

    fn read_line(&mut self) -> QueryResult<Option<String>> {
        let mut line = String::new();
        let read = self
            .reader
            .read_line(&mut line)
            .map_err(|error| load_csv_error(format!("LOAD CSV read failed: {error}")))?;
        if read == 0 {
            return Ok(None);
        }
        if self.first_record && line.starts_with('\u{feff}') {
            line.remove(0);
        }
        Ok(Some(line))
    }

    fn consume_line(&self, line: &str, state: &mut CsvRecordState) -> QueryResult<()> {
        state.saw_input = true;
        let mut chars = line.chars().peekable();
        while let Some(character) = chars.next() {
            if state.in_quotes {
                consume_quoted_character(character, &mut chars, state);
            } else {
                consume_outside_quotes(character, &mut chars, self.delimiter, state)?;
            }
            if state.complete {
                break;
            }
        }
        Ok(())
    }
}

#[derive(Default)]
struct CsvRecordState {
    fields: Vec<String>,
    field: String,
    in_quotes: bool,
    after_quote: bool,
    saw_input: bool,
    complete: bool,
}

impl CsvRecordState {
    fn finish_eof(mut self) -> QueryResult<Option<Vec<String>>> {
        if self.in_quotes {
            return Err(load_csv_error("LOAD CSV ended inside a quoted field"));
        }
        if !self.saw_input && self.field.is_empty() && self.fields.is_empty() {
            return Ok(None);
        }
        self.fields.push(self.field);
        Ok(Some(self.fields))
    }
}

fn consume_quoted_character(
    character: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    state: &mut CsvRecordState,
) {
    if character == '\\' && chars.peek() == Some(&'"') {
        chars.next();
        state.field.push('"');
        return;
    }
    if character != '"' {
        state.field.push(character);
        return;
    }
    if chars.peek() == Some(&'"') {
        chars.next();
        state.field.push('"');
    } else {
        state.in_quotes = false;
        state.after_quote = true;
    }
}

fn consume_outside_quotes(
    character: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    delimiter: char,
    state: &mut CsvRecordState,
) -> QueryResult<()> {
    if consume_delimiter_or_record_ending(character, chars, delimiter, state) {
        state.after_quote = false;
        return Ok(());
    }
    if state.after_quote {
        return Err(load_csv_error(
            "LOAD CSV has characters after a closing quote",
        ));
    }
    if character == '"' {
        if state.field.is_empty() {
            state.in_quotes = true;
        } else {
            return Err(load_csv_error(
                "LOAD CSV has an unexpected quote in an unquoted field",
            ));
        }
    } else {
        state.field.push(character);
    }
    Ok(())
}

fn consume_delimiter_or_record_ending(
    character: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    delimiter: char,
    state: &mut CsvRecordState,
) -> bool {
    if character == delimiter {
        state.fields.push(std::mem::take(&mut state.field));
        true
    } else {
        consume_record_ending(character, chars, state)
    }
}

fn consume_record_ending(
    character: char,
    chars: &mut std::iter::Peekable<std::str::Chars<'_>>,
    state: &mut CsvRecordState,
) -> bool {
    if character == '\r' {
        if chars.peek() == Some(&'\n') {
            chars.next();
        }
    } else if character != '\n' {
        return false;
    }
    state.fields.push(std::mem::take(&mut state.field));
    state.complete = true;
    true
}

fn load_csv_error(message: impl Into<String>) -> QueryError {
    QueryError::new(QueryErrorKind::Resource, message)
}
