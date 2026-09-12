use crate::cypher::{self, DurationValue, PointValue, TimeValue, Value, ZonedDateTimeValue};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

const NANOS_PER_SECOND: i128 = 1_000_000_000;

pub(super) fn value_property(value: &Value, key: &str) -> QueryResult<Value> {
    match value {
        Value::Date(value) => date_property(value.days(), key),
        Value::LocalTime(value) => time_property(value.nanoseconds(), None, key),
        Value::Time(value) => zoned_time_property(value, key),
        Value::LocalDateTime(value) => {
            let (days, nanoseconds) = value.storage_components();
            date_time_property(days, nanoseconds, key)
        }
        Value::ZonedDateTime(value) => zoned_datetime_property(value, key),
        Value::Duration(value) => duration_property(value, key),
        Value::Point(value) => point_property(value, key),
        _ => Err(property_error(key)),
    }
}

fn date_time_property(days: i64, nanoseconds: u64, key: &str) -> QueryResult<Value> {
    date_property(days, key).or_else(|error| {
        if error.kind == QueryErrorKind::Type {
            time_property(nanoseconds, None, key)
        } else {
            Err(error)
        }
    })
}

fn zoned_datetime_property(value: &ZonedDateTimeValue, key: &str) -> QueryResult<Value> {
    let (days, nanoseconds) = value.local_components()?;
    if let Ok(component) = date_property(days, key) {
        return Ok(component);
    }
    if let Ok(component) = time_property(nanoseconds, Some(value.offset_seconds()), key) {
        return Ok(component);
    }
    let instant = value.instant_nanoseconds();
    match key.to_ascii_lowercase().as_str() {
        "timezone" => Ok(Value::String(value.zone().to_owned())),
        "epochseconds" => integer_from_i128(instant.div_euclid(NANOS_PER_SECOND), key),
        "epochmillis" => integer_from_i128(instant.div_euclid(1_000_000), key),
        _ => Err(property_error(key)),
    }
}

fn zoned_time_property(value: &TimeValue, key: &str) -> QueryResult<Value> {
    let (nanoseconds, offset) = value.storage_components();
    time_property(nanoseconds, Some(offset), key)
}

fn date_property(days: i64, key: &str) -> QueryResult<Value> {
    let (year, month, day) = cypher::civil_from_days(days);
    let ordinal_day = days - cypher::days_from_civil(year, 1, 1) + 1;
    let day_of_week = (days + 3).rem_euclid(7) + 1;
    let quarter = (month - 1) / 3 + 1;
    let quarter_start = cypher::days_from_civil(year, (quarter - 1) * 3 + 1, 1);
    let (week_year, week) = iso_week(days, day_of_week);
    let component = match key.to_ascii_lowercase().as_str() {
        "year" => year,
        "quarter" => i64::from(quarter),
        "month" => i64::from(month),
        "week" => week,
        "weekyear" => week_year,
        "dayofquarter" | "quarterday" => days - quarter_start + 1,
        "day" => i64::from(day),
        "ordinalday" => ordinal_day,
        "dayofweek" | "weekday" => day_of_week,
        _ => return Err(property_error(key)),
    };
    Ok(Value::Integer(component))
}

fn iso_week(days: i64, day_of_week: i64) -> (i64, i64) {
    let thursday = days + (4 - day_of_week);
    let week_year = cypher::civil_from_days(thursday).0;
    let january_fourth = cypher::days_from_civil(week_year, 1, 4);
    let first_monday = january_fourth - (january_fourth + 3).rem_euclid(7);
    (week_year, (days - first_monday).div_euclid(7) + 1)
}

fn time_property(nanoseconds: u64, offset: Option<i32>, key: &str) -> QueryResult<Value> {
    let total_seconds = nanoseconds / NANOS_PER_SECOND as u64;
    let second_fraction = nanoseconds % NANOS_PER_SECOND as u64;
    let component = match key.to_ascii_lowercase().as_str() {
        "hour" => total_seconds / 3_600,
        "minute" => total_seconds / 60 % 60,
        "second" => total_seconds % 60,
        "millisecond" => second_fraction / 1_000_000,
        "microsecond" => second_fraction / 1_000,
        "nanosecond" => second_fraction,
        "offsetminutes" => return offset_integer(offset, 60, key),
        "offsetseconds" => return offset_integer(offset, 1, key),
        "offset" | "timezone" => {
            return offset
                .map(|value| Value::String(format_offset(value)))
                .ok_or_else(|| property_error(key));
        }
        _ => return Err(property_error(key)),
    };
    Ok(Value::Integer(component as i64))
}

fn offset_integer(offset: Option<i32>, divisor: i32, key: &str) -> QueryResult<Value> {
    offset
        .map(|value| Value::Integer(i64::from(value / divisor)))
        .ok_or_else(|| property_error(key))
}

fn format_offset(offset: i32) -> String {
    if offset == 0 {
        return "Z".to_owned();
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let absolute = offset.unsigned_abs();
    format!("{sign}{:02}:{:02}", absolute / 3_600, absolute % 3_600 / 60)
}

fn duration_property(value: &DurationValue, key: &str) -> QueryResult<Value> {
    let (months, days, seconds, nanoseconds) = value.components();
    let total_nanos = i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds);
    let component = match key.to_ascii_lowercase().as_str() {
        "years" => i128::from(months / 12),
        "quarters" => i128::from(months / 3),
        "quartersofyear" => i128::from(months % 12 / 3),
        "months" => i128::from(months),
        "monthsofyear" => i128::from(months % 12),
        "monthsofquarter" => i128::from(months % 3),
        "weeks" => i128::from(days / 7),
        "days" => i128::from(days),
        "daysofweek" => i128::from(days % 7),
        "hours" => i128::from(seconds / 3_600),
        "minutes" => i128::from(seconds / 60),
        "seconds" => i128::from(seconds),
        "milliseconds" => total_nanos / 1_000_000,
        "microseconds" => total_nanos / 1_000,
        "nanoseconds" => total_nanos,
        "minutesofhour" => i128::from(seconds % 3_600 / 60),
        "secondsofminute" => i128::from(seconds % 60),
        "millisecondsofsecond" => i128::from(nanoseconds / 1_000_000),
        "microsecondsofsecond" => i128::from(nanoseconds / 1_000),
        "nanosecondsofsecond" => i128::from(nanoseconds),
        _ => return Err(property_error(key)),
    };
    integer_from_i128(component, key)
}

fn point_property(value: &PointValue, key: &str) -> QueryResult<Value> {
    let coordinates = value.coordinates();
    let geographic = value.crs().starts_with("wgs-84");
    match key.to_ascii_lowercase().as_str() {
        "x" => Ok(Value::Float(coordinates[0])),
        "y" => Ok(Value::Float(coordinates[1])),
        "z" if coordinates.len() == 3 => Ok(Value::Float(coordinates[2])),
        "longitude" if geographic => Ok(Value::Float(coordinates[0])),
        "latitude" if geographic => Ok(Value::Float(coordinates[1])),
        "height" if geographic && coordinates.len() == 3 => Ok(Value::Float(coordinates[2])),
        "z" | "longitude" | "latitude" | "height" => Err(property_error(key)),
        "crs" => Ok(Value::String(value.crs().to_owned())),
        "srid" => Ok(Value::Integer(i64::from(value.srid()))),
        _ => Err(property_error(key)),
    }
}

fn integer_from_i128(value: i128, key: &str) -> QueryResult<Value> {
    i64::try_from(value).map(Value::Integer).map_err(|_| {
        QueryError::new(
            QueryErrorKind::Type,
            format!("property {key} is outside INTEGER64 range"),
        )
    })
}

fn property_error(key: &str) -> QueryError {
    QueryError::new(
        QueryErrorKind::Type,
        format!("property {key} is not available on this value"),
    )
}
