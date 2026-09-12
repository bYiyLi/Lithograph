use unicode_normalization::UnicodeNormalization;

use crate::cypher::Value;

use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{contains_null, require_arity};

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let result = match name {
        "char_length" | "character_length" => length(values),
        "lower" | "tolower" => unary_string(values, |value| value.to_lowercase()),
        "upper" | "toupper" => unary_string(values, |value| value.to_uppercase()),
        "trim" | "btrim" => trim(values, TrimSide::Both),
        "ltrim" => trim(values, TrimSide::Left),
        "rtrim" => trim(values, TrimSide::Right),
        "left" => side(values, true),
        "right" => side(values, false),
        "substring" => substring(values),
        "replace" => replace(values),
        "split" => split(values),
        "string.join" => join(values),
        "string.indexof" => index_of(values),
        "string.regexreplace" => regex_replace(values),
        "normalize" => normalize(values),
        _ => return None,
    };
    Some(result)
}

fn length(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::String(value) => Ok(Value::Integer(value.chars().count() as i64)),
        Value::Null => Ok(Value::Null),
        _ => string_type_error(),
    }
}

fn unary_string(values: &[Value], operation: impl FnOnce(&str) -> String) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::String(value) => Ok(Value::String(operation(value))),
        Value::Null => Ok(Value::Null),
        _ => string_type_error(),
    }
}

#[derive(Clone, Copy)]
enum TrimSide {
    Left,
    Right,
    Both,
}

fn trim(values: &[Value], side: TrimSide) -> QueryResult<Value> {
    require_arity(values, 1, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let Value::String(input) = &values[0] else {
        return string_type_error();
    };
    let output = match values.get(1) {
        None => match side {
            TrimSide::Left => input.trim_start().to_owned(),
            TrimSide::Right => input.trim_end().to_owned(),
            TrimSide::Both => input.trim().to_owned(),
        },
        Some(Value::String(characters)) => match side {
            TrimSide::Left => input
                .trim_start_matches(|value| characters.contains(value))
                .to_owned(),
            TrimSide::Right => input
                .trim_end_matches(|value| characters.contains(value))
                .to_owned(),
            TrimSide::Both => input
                .trim_matches(|value| characters.contains(value))
                .to_owned(),
        },
        Some(_) => return string_type_error(),
    };
    Ok(Value::String(output))
}

fn side(values: &[Value], left: bool) -> QueryResult<Value> {
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
    let characters = input.chars().collect::<Vec<_>>();
    let start = if left {
        0
    } else {
        characters.len().saturating_sub(length)
    };
    Ok(Value::String(
        characters[start..characters.len().min(start.saturating_add(length))]
            .iter()
            .collect(),
    ))
}

fn substring(values: &[Value]) -> QueryResult<Value> {
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
    let characters = input.chars().collect::<Vec<_>>();
    let end = match values.get(2) {
        None => characters.len(),
        Some(Value::Integer(length)) => {
            start.saturating_add(string_index(*length, "substring() length")?)
        }
        Some(_) => return integer_type_error(),
    }
    .min(characters.len());
    let start = start.min(characters.len());
    Ok(Value::String(
        characters[start..end.max(start)].iter().collect(),
    ))
}

fn replace(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 3, 4)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let (Value::String(input), Value::String(search), Value::String(replacement)) =
        (&values[0], &values[1], &values[2])
    else {
        return string_type_error();
    };
    let output = match values.get(3) {
        None => input.replace(search, replacement),
        Some(Value::Integer(limit)) => {
            let limit = usize::try_from(*limit).map_err(|_| {
                QueryError::new(
                    QueryErrorKind::Type,
                    "replace() limit must be a non-negative Integer",
                )
            })?;
            input.replacen(search, replacement, limit)
        }
        Some(_) => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "replace() limit must be a non-negative Integer",
            ));
        }
    };
    Ok(Value::String(output))
}

fn split(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let Value::String(input) = &values[0] else {
        return string_type_error();
    };
    let delimiters = match &values[1] {
        Value::String(value) => vec![value.as_str()],
        Value::List(values) => values
            .iter()
            .map(|value| match value {
                Value::String(value) => Ok(value.as_str()),
                _ => string_type_error(),
            })
            .collect::<QueryResult<Vec<_>>>()?,
        _ => return string_type_error(),
    };
    Ok(Value::List(
        split_with_delimiters(input, &delimiters)
            .into_iter()
            .map(Value::String)
            .collect(),
    ))
}

fn split_with_delimiters(input: &str, delimiters: &[&str]) -> Vec<String> {
    if delimiters.is_empty() {
        return vec![input.to_owned()];
    }
    if delimiters.iter().any(|delimiter| delimiter.is_empty()) {
        if input.is_empty() {
            return vec![String::new()];
        }
        return input
            .chars()
            .map(|character| character.to_string())
            .collect();
    }
    let mut output = Vec::new();
    let mut start = 0;
    while start <= input.len() {
        let next = delimiters
            .iter()
            .filter_map(|delimiter| {
                input[start..]
                    .find(delimiter)
                    .map(|offset| (start + offset, delimiter.len()))
            })
            .min_by_key(|(offset, _)| *offset);
        let Some((offset, length)) = next else {
            output.push(input[start..].to_owned());
            break;
        };
        output.push(input[start..offset].to_owned());
        start = offset + length;
    }
    output
}

fn join(values: &[Value]) -> QueryResult<Value> {
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
    let delimiter = match &values[1] {
        Value::String(value) => value,
        _ => return string_type_error(),
    };
    let items = items
        .iter()
        .filter_map(|value| match value {
            Value::String(value) => Some(Ok(value.as_str())),
            Value::Null => None,
            _ => Some(string_type_error()),
        })
        .collect::<QueryResult<Vec<_>>>()?;
    Ok(Value::String(items.join(delimiter)))
}

fn index_of(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let (Value::String(input), Value::String(search)) = (&values[0], &values[1]) else {
        return string_type_error();
    };
    let Some(byte_index) = input.find(search) else {
        return Ok(Value::Integer(-1));
    };
    Ok(Value::Integer(input[..byte_index].chars().count() as i64))
}

fn regex_replace(values: &[Value]) -> QueryResult<Value> {
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
    let pattern = regex::Regex::new(pattern)
        .map_err(|error| QueryError::semantic(format!("invalid regular expression: {error}")))?;
    Ok(Value::String(
        pattern
            .replace_all(input, replacement.as_str())
            .into_owned(),
    ))
}

fn normalize(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let Value::String(input) = &values[0] else {
        return string_type_error();
    };
    let form = match values.get(1) {
        None => "NFC",
        Some(Value::String(value)) => value.as_str(),
        Some(_) => return string_type_error(),
    };
    let output = match form.to_ascii_uppercase().as_str() {
        "NFC" => input.nfc().collect(),
        "NFD" => input.nfd().collect(),
        "NFKC" => input.nfkc().collect(),
        "NFKD" => input.nfkd().collect(),
        _ => return Err(QueryError::semantic("normalize() form is unsupported")),
    };
    Ok(Value::String(output))
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
