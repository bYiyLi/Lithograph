use std::cmp::Ordering;

use super::value::ValueError;

const NANOS_PER_SECOND: i128 = 1_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DateValue {
    text: String,
    days: i64,
}

impl DateValue {
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        let (year, month, day) = parse_date(text)?;
        Ok(Self {
            text: format_date(year, month, day),
            days: days_from_civil(year, month, day),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn days(&self) -> i64 {
        self.days
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalTimeValue {
    text: String,
    nanoseconds: u64,
}

impl LocalTimeValue {
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        let (nanoseconds, canonical) = parse_local_time(text)?;
        Ok(Self {
            text: canonical,
            nanoseconds,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn nanoseconds(&self) -> u64 {
        self.nanoseconds
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimeValue {
    text: String,
    local_nanoseconds: u64,
    offset_seconds: i32,
}

impl TimeValue {
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        let (local, offset_text) = split_time_offset(text)?;
        let (local_nanoseconds, local_text) = parse_local_time(local)?;
        let offset_seconds = parse_offset(offset_text)?;
        Ok(Self {
            text: format!("{local_text}{}", format_offset(offset_seconds)),
            local_nanoseconds,
            offset_seconds,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn instant_key(&self) -> i128 {
        i128::from(self.local_nanoseconds) - i128::from(self.offset_seconds) * NANOS_PER_SECOND
    }

    pub(crate) fn comparison_key(&self) -> (i128, i32) {
        (self.instant_key(), self.offset_seconds)
    }

    pub(super) fn offset_seconds(&self) -> i32 {
        self.offset_seconds
    }
}

pub use super::temporal_datetime::{LocalDateTimeValue, ZonedDateTimeValue};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DurationValue {
    text: String,
    months: i64,
    days: i64,
    seconds: i64,
    nanoseconds: i64,
}

impl DurationValue {
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        let (months, days, seconds, nanoseconds) = parse_duration(text)?;
        Ok(Self {
            text: super::temporal_format::format_duration(months, days, seconds, nanoseconds),
            months,
            days,
            seconds,
            nanoseconds,
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn components(&self) -> (i64, i64, i64, i64) {
        (self.months, self.days, self.seconds, self.nanoseconds)
    }
}

pub(crate) fn compare_time(left: &TimeValue, right: &TimeValue) -> Ordering {
    left.comparison_key().cmp(&right.comparison_key())
}

pub(crate) fn compare_zoned_datetime(
    left: &ZonedDateTimeValue,
    right: &ZonedDateTimeValue,
) -> Ordering {
    left.comparison_key().cmp(&right.comparison_key())
}

fn parse_date(text: &str) -> Result<(i64, u32, u32), ValueError> {
    let (year_month, day) = text
        .rsplit_once('-')
        .ok_or_else(|| ValueError::new("Date must use calendar YYYY-MM-DD form"))?;
    let (year, month) = year_month
        .rsplit_once('-')
        .ok_or_else(|| ValueError::new("Date must use calendar YYYY-MM-DD form"))?;
    let year = parse_year(year)?;
    let month = parse_two_digits(month, "Date month")?;
    let day = parse_two_digits(day, "Date day")?;
    if !(1..=12).contains(&month) {
        return Err(ValueError::new("Date month must be between 01 and 12"));
    }
    let maximum = days_in_month(year, month);
    if day == 0 || day > maximum {
        return Err(ValueError::new("Date day is outside the selected month"));
    }
    Ok((year, month, day))
}

fn parse_year(text: &str) -> Result<i64, ValueError> {
    let digits = text.strip_prefix(['+', '-']).unwrap_or(text);
    if digits.len() < 4 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ValueError::new(
            "Date year must contain at least four digits",
        ));
    }
    let year = text
        .parse::<i64>()
        .map_err(|_| ValueError::new("Date year is outside the supported range"))?;
    if !(-999_999_999..=999_999_999).contains(&year) {
        return Err(ValueError::new("Date year is outside the Cypher range"));
    }
    Ok(year)
}

fn parse_local_time(text: &str) -> Result<(u64, String), ValueError> {
    let mut parts = text.split(':');
    let hour = parse_two_digits(parts.next().unwrap_or_default(), "Time hour")?;
    let minute = parse_two_digits(parts.next().unwrap_or_default(), "Time minute")?;
    let second_text = parts
        .next()
        .ok_or_else(|| ValueError::new("Time must use HH:MM:SS form"))?;
    if parts.next().is_some() {
        return Err(ValueError::new("Time contains too many ':' separators"));
    }
    let (second, nanosecond) = parse_second(second_text)?;
    if hour > 23 || minute > 59 || second > 59 {
        return Err(ValueError::new(
            "Time components are outside their valid ranges",
        ));
    }
    let nanoseconds = (u64::from(hour) * 3_600 + u64::from(minute) * 60 + u64::from(second))
        * 1_000_000_000
        + u64::from(nanosecond);
    let canonical = if nanosecond == 0 {
        format!("{hour:02}:{minute:02}:{second:02}")
    } else {
        let fraction = format!("{nanosecond:09}").trim_end_matches('0').to_owned();
        format!("{hour:02}:{minute:02}:{second:02}.{fraction}")
    };
    Ok((nanoseconds, canonical))
}

fn parse_second(text: &str) -> Result<(u32, u32), ValueError> {
    let (second, fraction) = text
        .split_once('.')
        .map_or((text, None), |(second, fraction)| (second, Some(fraction)));
    let second = parse_two_digits(second, "Time second")?;
    let nanosecond = match fraction {
        None => 0,
        Some(value)
            if !value.is_empty()
                && value.len() <= 9
                && value.bytes().all(|byte| byte.is_ascii_digit()) =>
        {
            let parsed = value
                .parse::<u32>()
                .map_err(|_| ValueError::new("Time fractional second is invalid"))?;
            parsed * 10_u32.pow(9 - value.len() as u32)
        }
        Some(_) => {
            return Err(ValueError::new(
                "Time fractional second must contain one to nine digits",
            ));
        }
    };
    Ok((second, nanosecond))
}

fn split_time_offset(text: &str) -> Result<(&str, &str), ValueError> {
    if let Some(local) = text.strip_suffix('Z') {
        return Ok((local, "Z"));
    }
    let index = text
        .char_indices()
        .rev()
        .find_map(|(index, value)| matches!(value, '+' | '-').then_some(index))
        .ok_or_else(|| ValueError::new("Zoned Time requires a UTC offset"))?;
    if index == 0 {
        return Err(ValueError::new(
            "Zoned Time requires a local time before its offset",
        ));
    }
    Ok(text.split_at(index))
}

fn parse_offset(text: &str) -> Result<i32, ValueError> {
    if text == "Z" {
        return Ok(0);
    }
    let (sign, digits) = match text.as_bytes().first() {
        Some(b'+') => (1_i32, &text[1..]),
        Some(b'-') => (-1_i32, &text[1..]),
        _ => {
            return Err(ValueError::new(
                "Time zone offset must start with +, -, or Z",
            ));
        }
    };
    let (hour, minute) = if let Some((hour, minute)) = digits.split_once(':') {
        (
            parse_two_digits(hour, "Offset hour")?,
            parse_two_digits(minute, "Offset minute")?,
        )
    } else if digits.len() == 2 {
        (parse_two_digits(digits, "Offset hour")?, 0)
    } else if digits.len() == 4 {
        (
            parse_two_digits(&digits[..2], "Offset hour")?,
            parse_two_digits(&digits[2..], "Offset minute")?,
        )
    } else {
        return Err(ValueError::new(
            "Time zone offset must use ±HH, ±HHMM, or ±HH:MM",
        ));
    };
    if hour > 18 || minute > 59 || (hour == 18 && minute != 0) {
        return Err(ValueError::new(
            "Time zone offset is outside the ±18:00 range",
        ));
    }
    Ok(sign * (hour as i32 * 3_600 + minute as i32 * 60))
}

fn format_offset(offset_seconds: i32) -> String {
    if offset_seconds == 0 {
        return "Z".to_owned();
    }
    let sign = if offset_seconds < 0 { '-' } else { '+' };
    let absolute = offset_seconds.unsigned_abs();
    format!(
        "{sign}{:02}:{:02}",
        absolute / 3_600,
        (absolute % 3_600) / 60
    )
}

pub(super) fn validate_zone_id(zone: &str) -> Result<Option<i32>, ValueError> {
    if zone == "Z" {
        return Ok(Some(0));
    }
    if matches!(zone.as_bytes().first(), Some(b'+') | Some(b'-')) {
        return parse_offset(zone).map(Some);
    }
    if matches!(zone, "UTC" | "GMT" | "GMT0") {
        return Ok(Some(0));
    }
    if zone.is_empty()
        || zone.starts_with('/')
        || zone.ends_with('/')
        || zone.split('/').any(|part| {
            part.is_empty()
                || part == "."
                || part == ".."
                || !part.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'+' | b'.')
                })
        })
    {
        return Err(ValueError::new("ZonedDateTime zone id has invalid syntax"));
    }
    // IANA zone-rule lookup, alias resolution, and DST/offset validation are
    // execution semantics. Phase 03 preserves the exact syntactically valid
    // zone id without making the value model depend on an OS timezone database.
    Ok(None)
}

fn parse_duration(text: &str) -> Result<(i64, i64, i64, i64), ValueError> {
    let body = text
        .strip_prefix('P')
        .ok_or_else(|| ValueError::new("Duration must start with P"))?;
    if body.is_empty() {
        return Err(ValueError::new(
            "Duration must contain at least one component",
        ));
    }
    let mut months = 0_i64;
    let mut days = 0_i64;
    let mut seconds = 0_i64;
    let mut nanoseconds = 0_i64;
    let mut in_time = false;
    let mut start = 0_usize;
    let bytes = body.as_bytes();
    let mut index = 0_usize;
    let mut components = 0_usize;
    let mut time_components = 0_usize;
    let mut last_date_order = None;
    let mut last_time_order = None;
    while index < bytes.len() {
        if bytes[index] == b'T' {
            if in_time || start != index {
                return Err(ValueError::new("Duration T separator is misplaced"));
            }
            in_time = true;
            index += 1;
            start = index;
            continue;
        }
        if bytes[index].is_ascii_alphabetic() {
            if start == index {
                return Err(ValueError::new(
                    "Duration component is missing a numeric value",
                ));
            }
            let number = &body[start..index];
            let unit = bytes[index] as char;
            let order = duration_component_order(unit, in_time).ok_or_else(|| {
                ValueError::new("Duration component uses an invalid unit or position")
            })?;
            let last_order = if in_time {
                &mut last_time_order
            } else {
                &mut last_date_order
            };
            if last_order.is_some_and(|previous| order <= previous) {
                return Err(ValueError::new(
                    "Duration components must appear once each in canonical unit order",
                ));
            }
            *last_order = Some(order);
            apply_duration_component(
                number,
                unit,
                in_time,
                &mut months,
                &mut days,
                &mut seconds,
                &mut nanoseconds,
            )?;
            components += 1;
            if in_time {
                time_components += 1;
            }
            index += 1;
            start = index;
            continue;
        }
        if !(bytes[index].is_ascii_digit() || matches!(bytes[index], b'+' | b'-' | b'.')) {
            return Err(ValueError::new("Duration contains an invalid character"));
        }
        index += 1;
    }
    if start != bytes.len() || components == 0 || (in_time && time_components == 0) {
        return Err(ValueError::new("Duration has an incomplete component"));
    }
    Ok((months, days, seconds, nanoseconds))
}

fn duration_component_order(unit: char, in_time: bool) -> Option<u8> {
    match (in_time, unit) {
        (false, 'Y') => Some(0),
        (false, 'M') => Some(1),
        (false, 'W') => Some(2),
        (false, 'D') => Some(3),
        (true, 'H') => Some(0),
        (true, 'M') => Some(1),
        (true, 'S') => Some(2),
        _ => None,
    }
}

fn apply_duration_component(
    number: &str,
    unit: char,
    in_time: bool,
    months: &mut i64,
    days: &mut i64,
    seconds: &mut i64,
    nanoseconds: &mut i64,
) -> Result<(), ValueError> {
    if unit == 'S' && in_time {
        let (whole, nanos) = parse_decimal_seconds(number)?;
        *seconds = seconds
            .checked_add(whole)
            .ok_or_else(|| ValueError::new("Duration seconds overflow INTEGER64"))?;
        *nanoseconds = nanoseconds
            .checked_add(nanos)
            .ok_or_else(|| ValueError::new("Duration nanoseconds overflow INTEGER64"))?;
        return Ok(());
    }
    if number.contains('.') {
        return Err(ValueError::new(
            "Canonical Duration only permits a fractional seconds component",
        ));
    }
    let value = number
        .parse::<i64>()
        .map_err(|_| ValueError::new("Duration component is outside INTEGER64 range"))?;
    match (in_time, unit) {
        (false, 'Y') => checked_add_scaled(months, value, 12, "Duration months"),
        (false, 'M') => checked_add_scaled(months, value, 1, "Duration months"),
        (false, 'W') => checked_add_scaled(days, value, 7, "Duration days"),
        (false, 'D') => checked_add_scaled(days, value, 1, "Duration days"),
        (true, 'H') => checked_add_scaled(seconds, value, 3_600, "Duration seconds"),
        (true, 'M') => checked_add_scaled(seconds, value, 60, "Duration seconds"),
        _ => Err(ValueError::new(
            "Duration component uses an invalid unit or position",
        )),
    }
}

fn parse_decimal_seconds(text: &str) -> Result<(i64, i64), ValueError> {
    let (whole, fraction) = match text.split_once('.') {
        Some((whole, fraction)) => (whole, Some(fraction)),
        None => (text, None),
    };
    let whole = whole
        .parse::<i64>()
        .map_err(|_| ValueError::new("Duration seconds are outside INTEGER64 range"))?;
    let Some(fraction) = fraction else {
        return Ok((whole, 0));
    };
    if fraction.is_empty()
        || fraction.len() > 9
        || !fraction.bytes().all(|byte| byte.is_ascii_digit())
    {
        return Err(ValueError::new(
            "Duration fractional seconds must contain one to nine digits",
        ));
    }
    let magnitude = fraction
        .parse::<i64>()
        .map_err(|_| ValueError::new("Duration fractional seconds are invalid"))?
        * 10_i64.pow(9 - fraction.len() as u32);
    let nanos = if text.starts_with('-') {
        -magnitude
    } else {
        magnitude
    };
    Ok((whole, nanos))
}

fn checked_add_scaled(
    target: &mut i64,
    value: i64,
    scale: i64,
    name: &str,
) -> Result<(), ValueError> {
    let scaled = value
        .checked_mul(scale)
        .ok_or_else(|| ValueError::new(format!("{name} overflow INTEGER64")))?;
    *target = target
        .checked_add(scaled)
        .ok_or_else(|| ValueError::new(format!("{name} overflow INTEGER64")))?;
    Ok(())
}

fn parse_two_digits(text: &str, name: &str) -> Result<u32, ValueError> {
    if text.len() != 2 || !text.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ValueError::new(format!(
            "{name} must contain exactly two digits"
        )));
    }
    text.parse::<u32>()
        .map_err(|_| ValueError::new(format!("{name} is invalid")))
}

fn days_in_month(year: i64, month: u32) -> u32 {
    match month {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if is_leap_year(year) => 29,
        2 => 28,
        _ => 0,
    }
}

fn is_leap_year(year: i64) -> bool {
    year % 4 == 0 && (year % 100 != 0 || year % 400 == 0)
}

fn days_from_civil(year: i64, month: u32, day: u32) -> i64 {
    let year = year - i64::from(month <= 2);
    let era = if year >= 0 { year } else { year - 399 } / 400;
    let year_of_era = year - era * 400;
    let month = i64::from(month);
    let day = i64::from(day);
    let day_of_year = (153 * (month + if month > 2 { -3 } else { 9 }) + 2) / 5 + day - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

fn format_date(year: i64, month: u32, day: u32) -> String {
    if (0..=9999).contains(&year) {
        format!("{year:04}-{month:02}-{day:02}")
    } else if (-9999..0).contains(&year) {
        format!("-{:04}-{month:02}-{day:02}", year.unsigned_abs())
    } else if year > 9999 {
        format!("+{year}-{month:02}-{day:02}")
    } else {
        format!("{year}-{month:02}-{day:02}")
    }
}
