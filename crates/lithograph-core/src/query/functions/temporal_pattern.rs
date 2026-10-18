//! Streaming, preflighted temporal format rendering.

use std::iter::Peekable;
use std::str::Chars;

use crate::query::resources::{ExecutionControl, MemoryReservation, checked_add, checked_mul};
use crate::query::{QueryError, QueryResult};

type Token<'a> = dyn FnMut(char, usize) -> QueryResult<(String, bool)> + 'a;
type Emit<'a> = dyn FnMut(&str, usize) -> QueryResult<()> + 'a;

pub(crate) fn render(
    pattern: &str,
    control: Option<&ExecutionControl<'_>>,
    token_bytes: u64,
    result_memory: &mut Option<MemoryReservation>,
    mut token: impl FnMut(char, usize) -> QueryResult<(String, bool)>,
) -> QueryResult<String> {
    let mut length = 0_u64;
    visit(
        pattern,
        control,
        token_bytes,
        &mut token,
        &mut |text, count| {
            length = checked_add(length, checked_mul(text.len() as u64, count as u64)?)?;
            Ok(())
        },
    )?;
    let capacity = usize::try_from(length).map_err(|_| size_error())?;
    let bytes = checked_add(std::mem::size_of::<crate::cypher::Value>() as u64, length)?;
    let memory = control.map(|control| control.reserve(bytes)).transpose()?;
    let mut output = String::with_capacity(capacity);
    visit(
        pattern,
        control,
        token_bytes,
        &mut token,
        &mut |text, count| append(&mut output, text, count, control),
    )?;
    if let Some(control) = control {
        control.check()?;
    }
    *result_memory = memory;
    Ok(output)
}

fn visit(
    pattern: &str,
    control: Option<&ExecutionControl<'_>>,
    token_bytes: u64,
    token: &mut Token<'_>,
    emit: &mut Emit<'_>,
) -> QueryResult<()> {
    let mut characters = pattern.chars().peekable();
    while let Some(character) = characters.next() {
        work(control)?;
        if character == '\'' {
            quoted(&mut characters, control, emit)?;
        } else if character.is_ascii_alphabetic() {
            component(
                character,
                &mut characters,
                control,
                token_bytes,
                token,
                emit,
            )?;
        } else {
            literal(character, emit)?;
        }
    }
    Ok(())
}

fn component(
    character: char,
    characters: &mut Peekable<Chars<'_>>,
    control: Option<&ExecutionControl<'_>>,
    token_bytes: u64,
    token: &mut Token<'_>,
    emit: &mut Emit<'_>,
) -> QueryResult<()> {
    let width = consume_run(character, characters, control)?;
    let (character, width, pad) = if character == 'p' {
        let next = characters.next().ok_or_else(|| {
            QueryError::semantic("pad modifier requires a following pattern component")
        })?;
        work(control)?;
        if !next.is_ascii_alphabetic() || next == 'p' {
            return Err(QueryError::semantic(
                "pad modifier requires a following pattern component",
            ));
        }
        (next, consume_run(next, characters, control)?, Some(width))
    } else {
        (character, width, None)
    };
    // Token callbacks clip unbounded numeric widths before invoking the legacy
    // formatter. Their remaining allocations fit this input-derived bound.
    let _temporary = control
        .map(|control| control.reserve(token_bytes))
        .transpose()?;
    let (text, numeric) = token(character, width)?;
    if let Some(pad) = pad {
        let length = rendered_characters(&text, width, numeric, control)?;
        if length > pad {
            return Err(QueryError::semantic(
                "temporal component exceeds its requested pad width",
            ));
        }
        emit(" ", pad - length)?;
    }
    emit_component(&text, width, numeric, emit)
}

fn consume_run(
    character: char,
    characters: &mut Peekable<Chars<'_>>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<usize> {
    let mut count = 1;
    while characters.peek() == Some(&character) {
        characters.next();
        work(control)?;
        count += 1;
    }
    Ok(count)
}

fn rendered_characters(
    text: &str,
    width: usize,
    numeric: bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<usize> {
    let mut count = 0;
    for _ in text.chars() {
        work(control)?;
        count += 1;
    }
    Ok(if numeric && width > 2 {
        let negative = usize::from(text.starts_with('-'));
        width.max(count - negative) + negative
    } else {
        count
    })
}

fn emit_component(text: &str, width: usize, numeric: bool, emit: &mut Emit<'_>) -> QueryResult<()> {
    if !numeric || width <= 2 {
        return emit(text, 1);
    }
    let digits = if let Some(digits) = text.strip_prefix('-') {
        emit("-", 1)?;
        digits
    } else {
        text
    };
    emit("0", width.saturating_sub(digits.len()))?;
    emit(digits, 1)
}

fn quoted(
    characters: &mut Peekable<Chars<'_>>,
    control: Option<&ExecutionControl<'_>>,
    emit: &mut Emit<'_>,
) -> QueryResult<()> {
    if characters.peek() == Some(&'\'') {
        characters.next();
        work(control)?;
        return emit("'", 1);
    }
    while let Some(character) = characters.next() {
        work(control)?;
        if character == '\'' {
            if characters.peek() != Some(&'\'') {
                return Ok(());
            }
            characters.next();
            work(control)?;
            emit("'", 1)?;
        } else {
            let mut buffer = [0; 4];
            emit(character.encode_utf8(&mut buffer), 1)?;
        }
    }
    Err(QueryError::semantic("unterminated temporal format literal"))
}

fn literal(character: char, emit: &mut Emit<'_>) -> QueryResult<()> {
    if matches!(character, '[' | ']' | '{' | '}' | '#') {
        return Err(QueryError::semantic(
            "reserved temporal format character must be quoted",
        ));
    }
    let mut buffer = [0; 4];
    emit(character.encode_utf8(&mut buffer), 1)
}

pub(super) fn append(
    output: &mut String,
    text: &str,
    count: usize,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    if count == 0 {
        return Ok(());
    }
    if count > 1 {
        return append_padding(output, text, count, control);
    }
    super::super::text::append(output, text, control)
}

fn append_padding(
    output: &mut String,
    text: &str,
    mut count: usize,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    let chunk = match text {
        "0" => "0000000000000000000000000000000000000000000000000000000000000000",
        " " => "                                                                ",
        _ => {
            return Err(QueryError::internal(
                "non-padding temporal component was repeated",
            ));
        }
    };
    while count != 0 {
        work(control)?;
        let length = count.min(chunk.len());
        if let Some(control) = control {
            control.bytes(length as u64)?;
        }
        output.push_str(&chunk[..length]);
        count -= length;
    }
    Ok(())
}

fn work(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    control.map_or(Ok(()), |control| control.work(1))
}

fn size_error() -> QueryError {
    crate::query::resources::memory_error("temporal format output length overflow")
}
