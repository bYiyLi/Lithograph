use super::*;

pub(super) fn local_time_text(value: &str) -> String {
    let value = value.strip_prefix('T').unwrap_or(value).replace(',', ".");
    if value.contains(':') {
        return normalize_colon_time(value);
    }
    normalize_compact_time(value)
}

fn normalize_colon_time(value: String) -> String {
    let padded = match value.split_once(':') {
        Some((hour, rest)) if hour.len() == 1 && hour.bytes().all(|byte| byte.is_ascii_digit()) => {
            format!("0{hour}:{rest}")
        }
        _ => value,
    };
    if padded.matches(':').count() == 1 {
        format!("{padded}:00")
    } else {
        padded
    }
}

fn normalize_compact_time(value: String) -> String {
    let (digits, fraction) = match value.split_once('.') {
        Some((digits, fraction)) => (digits, Some(fraction)),
        None => (value.as_str(), None),
    };
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return value;
    }
    let clock = match digits.len() {
        2 => format!("{digits}:00:00"),
        4 => format!("{}:{}:00", &digits[..2], &digits[2..]),
        6 => format!("{}:{}:{}", &digits[..2], &digits[2..4], &digits[4..]),
        _ => return value,
    };
    match fraction {
        Some(fraction) => format!("{clock}.{fraction}"),
        None => clock,
    }
}

pub(super) fn offset_text(value: &str) -> String {
    if value.len() == 5 && !value.contains(':') {
        format!("{}:{}", &value[..3], &value[3..])
    } else if value.len() == 3 {
        format!("{value}:00")
    } else {
        value.to_owned()
    }
}

pub(super) fn local_datetime_text(value: &str) -> QueryResult<String> {
    let (date, time) = match value.split_once("T") {
        Some(parts) => parts,
        None => {
            return Err(QueryError::semantic(
                "datetime input requires date and time",
            ));
        }
    };
    let date = super::normalize_date_text(date)?;
    let time = local_time_text(time);
    Ok(date + "T" + &time)
}

pub(super) fn zoned_time_text(value: &str) -> String {
    if let Some(local) = value.strip_suffix('Z') {
        return format!("{}Z", local_time_text(local));
    }
    let offset = value
        .char_indices()
        .skip(1)
        .find(|(_, character)| matches!(character, '+' | '-'));
    let offset = match offset {
        Some((index, _)) => index,
        None => return value.to_owned(),
    };
    format!(
        "{}{}",
        local_time_text(&value[..offset]),
        offset_text(&value[offset..])
    )
}
