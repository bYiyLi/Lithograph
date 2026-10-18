use std::mem::size_of;

use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::{
    canonical_combining_class, decompose_canonical, decompose_compatible,
};

use crate::cypher::Value;
use crate::query::expression::NormalizationForm;

use super::super::resources::{
    ExecutionControl, MemoryReservation, checked_add, checked_mul, memory_error,
};
use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{build_value, contains_null, require_arity};

const TEXT_CHUNK: usize = 32 * 1024 - 4;

super::legacy_evaluators!();

pub(super) fn evaluate_with_memory(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    let result = match name {
        "char_length" | "character_length" => length(values, control),
        "lower" | "tolower" => case(values, false, control, result_memory),
        "upper" | "toupper" => case(values, true, control, result_memory),
        "trim" | "btrim" => trim(values, TrimSide::Both, control, result_memory),
        "ltrim" => trim(values, TrimSide::Left, control, result_memory),
        "rtrim" => trim(values, TrimSide::Right, control, result_memory),
        "left" => side(values, true, control, result_memory),
        "right" => side(values, false, control, result_memory),
        "substring" => substring(values, control, result_memory),
        "replace" => replace(values, control, result_memory),
        "split" => split(values, control, result_memory),
        "string.join" => join(values, control, result_memory),
        "string.indexof" => index_of(values, control),
        "string.regexreplace" => regex_replace(values, control, result_memory),
        "normalize" => normalize(values, control, result_memory),
        _ => return None,
    };
    Some(result)
}

fn check(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    if let Some(control) = control {
        control.check()?;
    }
    Ok(())
}

fn work(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    if let Some(control) = control {
        control.work(1)?;
    }
    Ok(())
}

fn reserve(
    control: Option<&ExecutionControl<'_>>,
    bytes: u64,
) -> QueryResult<Option<MemoryReservation>> {
    control.map(|control| control.reserve(bytes)).transpose()
}

fn string_buffer(length: usize) -> QueryResult<String> {
    let mut output = String::new();
    output
        .try_reserve_exact(length)
        .map_err(|_| memory_error("cannot allocate String function result"))?;
    Ok(output)
}

fn output_bytes(length: usize) -> QueryResult<u64> {
    checked_add(size_of::<Value>() as u64, length as u64)
}

fn chunk_end(input: &str, start: usize, maximum: usize) -> usize {
    let mut end = start.saturating_add(maximum).min(input.len());
    while !input.is_char_boundary(end) {
        end -= 1;
    }
    end
}

pub(crate) fn append(
    output: &mut String,
    input: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    let mut offset = 0;
    while offset < input.len() {
        let end = chunk_end(input, offset, TEXT_CHUNK);
        if let Some(control) = control {
            control.bytes((end - offset) as u64)?;
        }
        output.push_str(&input[offset..end]);
        offset = end;
    }
    Ok(())
}

fn copy(
    input: &str,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    build_string(input.len(), control, result_memory, |output| {
        append(output, input, control)
    })
}

fn build_string(
    length: usize,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
    fill: impl FnOnce(&mut String) -> QueryResult<()>,
) -> QueryResult<Value> {
    build_value(output_bytes(length)?, control, result_memory, || {
        let mut output = string_buffer(length)?;
        fill(&mut output)?;
        Ok(Value::String(output))
    })
}

fn materialize_string(
    mut visit: impl FnMut(&mut dyn FnMut(&str) -> QueryResult<()>) -> QueryResult<()>,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let mut bytes = 0_u64;
    visit(&mut |part| {
        bytes = checked_add(bytes, part.len() as u64)?;
        Ok(())
    })?;
    build_string(bytes as usize, control, result_memory, |output| {
        visit(&mut |part| append(output, part, control))
    })
}

fn count_characters(input: &str, control: Option<&ExecutionControl<'_>>) -> QueryResult<usize> {
    let mut count = 0;
    for _ in input.chars() {
        work(control)?;
        count += 1;
    }
    Ok(count)
}

fn length(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::String(value) => Ok(Value::Integer(count_characters(value, control)? as i64)),
        Value::Null => Ok(Value::Null),
        _ => string_type_error(),
    }
}

fn case(
    values: &[Value],
    upper: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    let input = match &values[0] {
        Value::String(value) => value,
        Value::Null => return Ok(Value::Null),
        _ => return string_type_error(),
    };
    if !upper {
        return lowercase(input, control, result_memory);
    }
    let mut length = 0_u64;
    for character in input.chars() {
        work(control)?;
        for mapped in character.to_uppercase() {
            length = checked_add(length, mapped.len_utf8() as u64)?;
        }
    }
    build_string(length as usize, control, result_memory, |output| {
        for character in input.chars() {
            work(control)?;
            output.extend(character.to_uppercase());
        }
        Ok(())
    })
}

fn lowercase(
    input: &str,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    // The standard lowercase operation has context-sensitive Greek sigma
    // handling. Preserve it rather than lowering each character independently.
    let mut length = 0_u64;
    for character in input.chars() {
        work(control)?;
        for mapped in character.to_lowercase() {
            length = checked_add(length, mapped.len_utf8() as u64)?;
        }
    }
    // The standard builder starts with the input's byte capacity, including
    // contractions such as the three-byte Kelvin sign becoming one-byte k.
    // Twice the final payload covers subsequent builder capacity growth.
    let payload_capacity = (input.len() as u64).max(checked_mul(length, 2)?);
    let mut reservation = reserve(
        control,
        checked_add(size_of::<Value>() as u64, payload_capacity)?,
    )?;
    check(control)?;
    let output = input.to_lowercase();
    check(control)?;
    if let Some(reservation) = &mut reservation {
        reservation.resize(output_bytes(output.capacity())?)?;
    }
    *result_memory = reservation;
    Ok(Value::String(output))
}

#[derive(Clone, Copy)]
enum TrimSide {
    Left,
    Right,
    Both,
}

fn trim(
    values: &[Value],
    side: TrimSide,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    with_nullable_text(values, 1, 2, |input| {
        let characters = match values.get(1) {
            None => None,
            Some(Value::String(characters)) => Some(characters.as_str()),
            Some(_) => return string_type_error(),
        };
        let mut start = 0;
        let mut end = input.len();
        if matches!(side, TrimSide::Left | TrimSide::Both) {
            for character in input.chars() {
                if !is_trim_character(character, characters, control)? {
                    break;
                }
                start += character.len_utf8();
            }
        }
        if matches!(side, TrimSide::Right | TrimSide::Both) {
            for character in input[start..].chars().rev() {
                if !is_trim_character(character, characters, control)? {
                    break;
                }
                end -= character.len_utf8();
            }
        }
        copy(&input[start..end], control, result_memory)
    })
}

fn is_trim_character(
    character: char,
    characters: Option<&str>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    work(control)?;
    let Some(characters) = characters else {
        return Ok(character.is_whitespace());
    };
    for candidate in characters.chars() {
        work(control)?;
        if character == candidate {
            return Ok(true);
        }
    }
    Ok(false)
}

fn character_offset(
    input: &str,
    index: usize,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<usize> {
    for (count, (offset, _)) in input.char_indices().enumerate() {
        work(control)?;
        if count == index {
            return Ok(offset);
        }
    }
    Ok(input.len())
}

fn side(
    values: &[Value],
    left: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if matches!(values[0], Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::String(input), Value::Integer(length)) = (&values[0], &values[1]) else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "left()/right() require String and Integer",
        ));
    };
    let length = string_index(*length, "String length")?;
    if left {
        let end = character_offset(input, length, control)?;
        copy(&input[..end], control, result_memory)
    } else {
        let count = count_characters(input, control)?;
        let start = character_offset(input, count.saturating_sub(length), control)?;
        copy(&input[start..], control, result_memory)
    }
}

fn substring(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 2, 3)?;
    if matches!(values[0], Value::Null) {
        return Ok(Value::Null);
    }
    let Value::String(input) = &values[0] else {
        return string_type_error();
    };
    let Value::Integer(start) = values[1] else {
        return integer_type_error();
    };
    let start = string_index(start, "substring() start")?;
    let length = match values.get(2) {
        None => None,
        Some(Value::Integer(length)) => Some(string_index(*length, "substring() length")?),
        Some(_) => return integer_type_error(),
    };
    let start = character_offset(input, start, control)?;
    let end = match length {
        None => input.len(),
        Some(length) => start + character_offset(&input[start..], length, control)?,
    };
    copy(&input[start..end], control, result_memory)
}

// Keep each standard-library search window within 64 KiB. A longer pattern uses
// a budgeted prefix table, preserving linear search while allowing checkpoints.
pub(crate) fn find(
    input: &str,
    search: &str,
    start: usize,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<usize>> {
    if search.is_empty() {
        return Ok(Some(start));
    }
    if search.len() <= TEXT_CHUNK {
        return find_windowed(input, search, start, control);
    }
    let (prefixes, _reservation) = search_prefixes(search.as_bytes(), control)?;
    let pattern = search.as_bytes();
    let mut matched = 0;
    for (index, byte) in input.as_bytes().iter().enumerate().skip(start) {
        work(control)?;
        while matched > 0 && *byte != pattern[matched] {
            work(control)?;
            matched = prefixes[matched - 1];
        }
        if *byte == pattern[matched] {
            matched += 1;
            if matched == pattern.len() {
                return Ok(Some(index + 1 - matched));
            }
        }
    }
    Ok(None)
}

fn find_windowed(
    input: &str,
    search: &str,
    start: usize,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<usize>> {
    let mut offset = start;
    while offset < input.len() {
        let boundary = chunk_end(input, offset, TEXT_CHUNK);
        let mut end = boundary.saturating_add(search.len() - 1).min(input.len());
        while !input.is_char_boundary(end) {
            end += 1;
        }
        check(control)?;
        let found = input[offset..end].find(search);
        if let Some(control) = control {
            control.bytes((end - offset) as u64)?;
        }
        if let Some(found) = found {
            return Ok(Some(offset + found));
        }
        offset = boundary;
    }
    Ok(None)
}

fn search_prefixes(
    pattern: &[u8],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(Vec<usize>, Option<MemoryReservation>)> {
    let reservation = reserve(
        control,
        checked_mul(pattern.len() as u64, size_of::<usize>() as u64)?,
    )?;
    let mut prefixes = Vec::new();
    prefixes
        .try_reserve_exact(pattern.len())
        .map_err(|_| memory_error("cannot allocate text search prefix table"))?;
    prefixes.push(0);
    let mut matched = 0;
    for index in 1..pattern.len() {
        work(control)?;
        while matched > 0 && pattern[index] != pattern[matched] {
            work(control)?;
            matched = prefixes[matched - 1];
        }
        if pattern[index] == pattern[matched] {
            matched += 1;
        }
        prefixes.push(matched);
    }
    Ok((prefixes, reservation))
}

fn replace(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 3, 4)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let (Value::String(input), Value::String(search), Value::String(replacement)) =
        (&values[0], &values[1], &values[2])
    else {
        return string_type_error();
    };
    let limit = match values.get(3) {
        None => usize::MAX,
        Some(Value::Integer(limit)) => usize::try_from(*limit).map_err(|_| {
            QueryError::new(
                QueryErrorKind::Type,
                "replace() limit must be a non-negative Integer",
            )
        })?,
        Some(_) => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "replace() limit must be a non-negative Integer",
            ));
        }
    };
    materialize_string(
        |emit| visit_replacements(input, search, replacement, limit, control, emit),
        control,
        result_memory,
    )
}

fn visit_replacements(
    input: &str,
    search: &str,
    replacement: &str,
    limit: usize,
    control: Option<&ExecutionControl<'_>>,
    emit: &mut dyn FnMut(&str) -> QueryResult<()>,
) -> QueryResult<()> {
    let mut start = 0;
    let mut count = 0;
    while count < limit {
        work(control)?;
        let Some(offset) = find(input, search, start, control)? else {
            break;
        };
        emit(&input[start..offset])?;
        emit(replacement)?;
        count += 1;
        start = offset + search.len();
        if search.is_empty() {
            let Some(character) = input[start..].chars().next() else {
                break;
            };
            let end = start + character.len_utf8();
            emit(&input[start..end])?;
            start = end;
        }
    }
    emit(&input[start..])
}

fn split(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let Some(input) = nullable_text_input(values, 2, 2)? else {
        return Ok(Value::Null);
    };
    let (delimiters, _delimiters_reservation) = split_delimiters(&values[1], control)?;
    let (count, bytes) = split_size(input, &delimiters, control)?;
    let reservation = reserve(control, bytes)?;
    let mut output = Vec::new();
    output
        .try_reserve_exact(count as usize)
        .map_err(|_| memory_error("cannot allocate split() result"))?;
    split_with_delimiters(input, &delimiters, control, &mut |part| {
        let mut string = string_buffer(part.len())?;
        append(&mut string, part, control)?;
        output.push(Value::String(string));
        Ok(())
    })?;
    *result_memory = reservation;
    Ok(Value::List(output))
}

fn split_delimiters<'a>(
    value: &'a Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(Vec<&'a str>, Option<MemoryReservation>)> {
    let delimiter_count = match value {
        Value::String(_) => 1,
        Value::List(values) => values.len(),
        _ => return string_type_error(),
    };
    let reservation = reserve(
        control,
        checked_mul(delimiter_count as u64, size_of::<&str>() as u64)?,
    )?;
    let mut delimiters = Vec::new();
    delimiters
        .try_reserve_exact(delimiter_count)
        .map_err(|_| memory_error("cannot allocate split() delimiters"))?;
    match value {
        Value::String(delimiter) => delimiters.push(delimiter.as_str()),
        Value::List(values) => {
            for value in values {
                work(control)?;
                delimiters.push(string_value(value)?);
            }
        }
        _ => unreachable!(),
    }
    Ok((delimiters, reservation))
}

fn string_value(value: &Value) -> QueryResult<&str> {
    match value {
        Value::String(value) => Ok(value),
        _ => string_type_error(),
    }
}

fn nullable_text_input(
    values: &[Value],
    minimum: usize,
    maximum: usize,
) -> QueryResult<Option<&str>> {
    require_arity(values, minimum, maximum)?;
    if contains_null(values) {
        Ok(None)
    } else {
        string_value(&values[0]).map(Some)
    }
}

fn with_nullable_text(
    values: &[Value],
    minimum: usize,
    maximum: usize,
    operation: impl FnOnce(&str) -> QueryResult<Value>,
) -> QueryResult<Value> {
    match nullable_text_input(values, minimum, maximum)? {
        Some(input) => operation(input),
        None => Ok(Value::Null),
    }
}

fn split_size(
    input: &str,
    delimiters: &[&str],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(u64, u64)> {
    let mut count = 0_u64;
    let mut payload = 0_u64;
    split_with_delimiters(input, delimiters, control, &mut |part| {
        count = checked_add(count, 1)?;
        payload = checked_add(payload, part.len() as u64)?;
        Ok(())
    })?;
    let bytes = checked_add(
        checked_add(payload, checked_mul(count, size_of::<Value>() as u64)?)?,
        size_of::<Value>() as u64,
    )?;
    Ok((count, bytes))
}

fn split_with_delimiters(
    input: &str,
    delimiters: &[&str],
    control: Option<&ExecutionControl<'_>>,
    emit: &mut dyn FnMut(&str) -> QueryResult<()>,
) -> QueryResult<()> {
    if delimiters.is_empty() {
        return emit(input);
    }
    if delimiters.iter().any(|delimiter| delimiter.is_empty()) {
        if input.is_empty() {
            return emit("");
        }
        for (start, character) in input.char_indices() {
            work(control)?;
            emit(&input[start..start + character.len_utf8()])?;
        }
        return Ok(());
    }
    let mut start = 0;
    while start <= input.len() {
        let mut next = None;
        for delimiter in delimiters {
            work(control)?;
            if let Some(offset) = find(input, delimiter, start, control)?
                && next.is_none_or(|(current, _)| offset < current)
            {
                next = Some((offset, delimiter.len()));
            }
        }
        let Some((offset, length)) = next else {
            emit(&input[start..])?;
            break;
        };
        emit(&input[start..offset])?;
        start = offset + length;
    }
    Ok(())
}

fn join(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let Value::List(items) = &values[0] else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "string.join() requires a List of String values",
        ));
    };
    let Value::String(delimiter) = &values[1] else {
        return string_type_error();
    };
    let length = joined_size(items, delimiter, control)?;
    build_string(length as usize, control, result_memory, |output| {
        append_joined(output, items, delimiter, control)
    })
}

fn joined_size(
    items: &[Value],
    delimiter: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<u64> {
    let mut length = 0_u64;
    let mut count = 0_u64;
    for item in items {
        work(control)?;
        match item {
            Value::String(item) => {
                length = checked_add(length, item.len() as u64)?;
                count += 1;
            }
            Value::Null => {}
            _ => return string_type_error(),
        }
    }
    checked_add(
        length,
        checked_mul(count.saturating_sub(1), delimiter.len() as u64)?,
    )
}

fn append_joined(
    output: &mut String,
    items: &[Value],
    delimiter: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
    let mut first = true;
    for item in items {
        work(control)?;
        if let Value::String(item) = item {
            if !first {
                append(output, delimiter, control)?;
            }
            append(output, item, control)?;
            first = false;
        }
    }
    Ok(())
}

fn index_of(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
    let Some(input) = nullable_text_input(values, 2, 2)? else {
        return Ok(Value::Null);
    };
    let search = string_value(&values[1])?;
    let Some(byte_index) = find(input, search, 0, control)? else {
        return Ok(Value::Integer(-1));
    };
    Ok(Value::Integer(
        count_characters(&input[..byte_index], control)? as i64,
    ))
}

fn regex_replace(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_arity(values, 3, 3)?;
    let [
        Value::String(input),
        Value::String(pattern),
        Value::String(replacement),
    ] = values
    else {
        if contains_null(values) {
            return Ok(Value::Null);
        }
        return string_type_error();
    };
    let (pattern, _pattern_reservation) = compile_regex_controlled(pattern, control)?;
    let _captures_reservation = reserve(
        control,
        checked_mul(
            pattern.captures_len() as u64,
            (4 * size_of::<usize>()) as u64,
        )?,
    )?;
    materialize_string(
        |emit| visit_regex_replacements(&pattern, input, replacement, control, emit),
        control,
        result_memory,
    )
}

fn visit_regex_replacements(
    pattern: &regex::Regex,
    input: &str,
    replacement: &str,
    control: Option<&ExecutionControl<'_>>,
    emit: &mut dyn FnMut(&str) -> QueryResult<()>,
) -> QueryResult<()> {
    let mut start = 0;
    let mut matches = pattern.captures_iter(input);
    loop {
        check(control)?;
        let next = matches.next();
        check(control)?;
        let Some(captures) = next else { break };
        let matched = captures.get(0).ok_or_else(|| {
            QueryError::internal("regular expression omitted its whole-match capture")
        })?;
        emit(&input[start..matched.start()])?;
        expand_capture(&captures, replacement, control, emit)?;
        start = matched.end();
    }
    emit(&input[start..])
}

pub(crate) fn compile_regex_controlled(
    pattern: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(regex::Regex, Option<MemoryReservation>)> {
    // Regex compilation/search is a third-party non-preemptible boundary.
    // Bound compiled state and the independent lazy-DFA cache, with a temporary
    // allowance for parser nodes and compilation state before entering it.
    let (compile_limit, dfa_limit, temporary) = regex_allowances(pattern.len(), control)?;
    let pattern_reservation = reserve(control, checked_add(compile_limit, dfa_limit)?)?;
    let _compile_memory = reserve(control, temporary)?;
    check(control)?;
    let pattern = regex::RegexBuilder::new(pattern)
        .size_limit(compile_limit as usize)
        .dfa_size_limit(dfa_limit as usize)
        .build()
        .map_err(|error| match error {
            regex::Error::CompiledTooBig(_)
                if control.is_some_and(|control| control.resources().limit().is_some()) =>
            {
                memory_error("regular expression exceeds execution memory budget")
            }
            _ => QueryError::semantic(format!("invalid regular expression: {error}")),
        })?;
    check(control)?;
    Ok((pattern, pattern_reservation))
}

fn regex_allowances(
    pattern_length: usize,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(u64, u64, u64)> {
    let parser = checked_add(checked_mul(pattern_length as u64, 64)?, 1024)?;
    let compile_limit = control
        .and_then(|control| {
            control.resources().limit().map(|limit| {
                (limit
                    .saturating_sub(control.resources().used())
                    .saturating_sub(parser)
                    / 3)
                .min(10 * 1024 * 1024)
            })
        })
        .unwrap_or(10 * 1024 * 1024);
    let dfa_limit = compile_limit.min(2 * 1024 * 1024);
    Ok((
        compile_limit,
        dfa_limit,
        checked_add(parser, compile_limit)?,
    ))
}

fn expand_capture(
    captures: &regex::Captures<'_>,
    replacement: &str,
    control: Option<&ExecutionControl<'_>>,
    emit: &mut dyn FnMut(&str) -> QueryResult<()>,
) -> QueryResult<()> {
    let mut start = 0;
    while let Some(dollar) = find(replacement, "$", start, control)? {
        work(control)?;
        emit(&replacement[start..dollar])?;
        let suffix = &replacement[dollar + 1..];
        if suffix.starts_with('$') {
            emit("$")?;
            start = dollar + 2;
            continue;
        }
        let reference = if suffix.starts_with('{') {
            find(suffix, "}", 1, control)?.map(|end| (&suffix[1..end], end + 1))
        } else {
            let mut end = 0;
            for byte in suffix.bytes() {
                work(control)?;
                if !(byte.is_ascii_alphanumeric() || byte == b'_') {
                    break;
                }
                end += 1;
            }
            (end > 0).then_some((&suffix[..end], end))
        };
        let Some((name, end)) = reference else {
            emit("$")?;
            start = dollar + 1;
            continue;
        };
        let captured = match name.parse::<usize>() {
            Ok(index) => captures.get(index),
            Err(_) => captures.name(name),
        };
        if let Some(captured) = captured {
            emit(captured.as_str())?;
        }
        start = dollar + 1 + end;
    }
    emit(&replacement[start..])
}

fn normalize(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    with_nullable_text(values, 1, 2, |input| {
        let form = match values.get(1) {
            None => "NFC",
            Some(Value::String(value)) => value.as_str(),
            Some(_) => return string_type_error(),
        };
        let (compatible, composed) = if form.eq_ignore_ascii_case("NFC") {
            (false, true)
        } else if form.eq_ignore_ascii_case("NFD") {
            (false, false)
        } else if form.eq_ignore_ascii_case("NFKC") {
            (true, true)
        } else if form.eq_ignore_ascii_case("NFKD") {
            (true, false)
        } else {
            return Err(QueryError::semantic("normalize() form is unsupported"));
        };
        let _scratch_reservation = normalization_scratch(input, compatible, control)?;
        let visit = |emit: &mut dyn FnMut(char) -> QueryResult<()>| -> QueryResult<()> {
            match (compatible, composed) {
                (false, true) => visit_normalized(input.nfc(), control, emit),
                (false, false) => visit_normalized(input.nfd(), control, emit),
                (true, true) => visit_normalized(input.nfkc(), control, emit),
                (true, false) => visit_normalized(input.nfkd(), control, emit),
            }
        };
        let mut length = 0_u64;
        visit(&mut |character| {
            length = checked_add(length, character.len_utf8() as u64)?;
            Ok(())
        })?;
        build_string(length as usize, control, result_memory, |output| {
            visit(&mut |character| {
                output.push(character);
                Ok(())
            })
        })
    })
}

fn normalization_scratch(
    input: &str,
    compatible: bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Option<MemoryReservation>> {
    // Unicode normalization retains the current combining sequence internally.
    // Count its decomposed upper bound before entering that library iterator.
    let mut sequence = 0_u64;
    let mut maximum = 0_u64;
    for character in input.chars() {
        work(control)?;
        let mut count = |decomposed| {
            if canonical_combining_class(decomposed) == 0 {
                sequence = 0;
            }
            sequence = sequence.saturating_add(1);
            maximum = maximum.max(sequence);
        };
        if compatible {
            decompose_compatible(character, &mut count);
        } else {
            decompose_canonical(character, &mut count);
        }
    }
    reserve(control, checked_mul(maximum, 32)?)
}

pub(crate) fn normalized_predicate_controlled(
    input: &str,
    form: NormalizationForm,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    let compatible = matches!(form, NormalizationForm::Nfkc | NormalizationForm::Nfkd);
    let _scratch_reservation = normalization_scratch(input, compatible, control)?;
    match form {
        NormalizationForm::Nfc => matches_normalized(input, input.nfc(), control),
        NormalizationForm::Nfd => matches_normalized(input, input.nfd(), control),
        NormalizationForm::Nfkc => matches_normalized(input, input.nfkc(), control),
        NormalizationForm::Nfkd => matches_normalized(input, input.nfkd(), control),
    }
}

fn matches_normalized(
    input: &str,
    mut normalized: impl Iterator<Item = char>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    let mut original = input.chars();
    loop {
        work(control)?;
        check(control)?;
        let normalized = normalized.next();
        check(control)?;
        match (original.next(), normalized) {
            (None, None) => return Ok(true),
            (Some(original), Some(normalized)) if original == normalized => {}
            _ => return Ok(false),
        }
    }
}

fn visit_normalized(
    mut characters: impl Iterator<Item = char>,
    control: Option<&ExecutionControl<'_>>,
    emit: &mut dyn FnMut(char) -> QueryResult<()>,
) -> QueryResult<()> {
    loop {
        check(control)?;
        let next = characters.next();
        check(control)?;
        let Some(character) = next else { break };
        emit(character)?;
    }
    Ok(())
}

fn string_index(value: i64, name: &str) -> QueryResult<usize> {
    if (0..=i64::from(i32::MAX)).contains(&value) {
        Ok(value as usize)
    } else {
        Err(QueryError::new(
            QueryErrorKind::Type,
            format!("{name} must be between 0 and INTEGER32 maximum"),
        ))
    }
}

fn string_type_error<T>() -> QueryResult<T> {
    Err(QueryError::new(
        QueryErrorKind::Type,
        "String function requires String input",
    ))
}

fn integer_type_error<T>() -> QueryResult<T> {
    Err(QueryError::new(
        QueryErrorKind::Type,
        "String index must be Integer",
    ))
}

#[cfg(test)]
#[path = "text/tests.rs"]
mod tests;
