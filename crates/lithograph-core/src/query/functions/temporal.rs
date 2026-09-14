use std::collections::BTreeMap;

use crate::cypher::{
    DateValue, DurationValue, LocalDateTimeValue, LocalTimeValue, TimeValue, Value,
    ZonedDateTimeValue,
};

use super::super::expression::BinaryOp;
use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::format_components::{
    format_era, format_localized_offset, format_quarter, format_weekday, format_year,
    iso_week_parts, month_name, period_of_day, year_of_era, zone_id, zone_name,
};
use super::{contains_null, require_arity};

const NANOS_PER_SECOND: i128 = 1_000_000_000;
const NANOS_PER_DAY: i128 = 86_400 * NANOS_PER_SECOND;

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let result = match name {
        "format" => format_value(values),
        "date.truncate" => truncate(values, TemporalFamily::Date),
        "localtime.truncate" => truncate(values, TemporalFamily::LocalTime),
        "time.truncate" => truncate(values, TemporalFamily::Time),
        "localdatetime.truncate" => truncate(values, TemporalFamily::LocalDateTime),
        "datetime.truncate" => truncate(values, TemporalFamily::ZonedDateTime),
        "datetime.fromepoch" => datetime_from_epoch(values),
        "datetime.fromepochmillis" => datetime_from_epoch_millis(values),
        "duration.between" | "duration_between" => duration_difference(values, Difference::Logical),
        "duration.inmonths" => duration_difference(values, Difference::Months),
        "duration.indays" => duration_difference(values, Difference::Days),
        "duration.inseconds" => duration_difference(values, Difference::Seconds),
        _ => return None,
    };
    Some(result)
}

pub(crate) fn evaluate_arithmetic(
    op: BinaryOp,
    left: &Value,
    right: &Value,
) -> Option<QueryResult<Value>> {
    let result = match (left, right) {
        (Value::Duration(left), Value::Duration(right))
            if matches!(op, BinaryOp::Add | BinaryOp::Subtract) =>
        {
            combine_duration(left, right, op == BinaryOp::Subtract).map(Value::Duration)
        }
        (Value::Duration(duration), Value::Integer(scale)) if op == BinaryOp::Multiply => {
            scale_duration(duration, *scale as f64).map(Value::Duration)
        }
        (Value::Duration(duration), Value::Float(scale)) if op == BinaryOp::Multiply => {
            scale_duration(duration, *scale).map(Value::Duration)
        }
        (Value::Integer(scale), Value::Duration(duration)) if op == BinaryOp::Multiply => {
            scale_duration(duration, *scale as f64).map(Value::Duration)
        }
        (Value::Float(scale), Value::Duration(duration)) if op == BinaryOp::Multiply => {
            scale_duration(duration, *scale).map(Value::Duration)
        }
        (Value::Duration(duration), Value::Integer(divisor)) if op == BinaryOp::Divide => {
            divide_duration(duration, *divisor as f64).map(Value::Duration)
        }
        (Value::Duration(duration), Value::Float(divisor)) if op == BinaryOp::Divide => {
            divide_duration(duration, *divisor).map(Value::Duration)
        }
        (temporal, Value::Duration(duration))
            if matches!(op, BinaryOp::Add | BinaryOp::Subtract) =>
        {
            shift_temporal(temporal, duration, op == BinaryOp::Subtract)
        }
        (Value::Duration(duration), temporal) if op == BinaryOp::Add => {
            shift_temporal(temporal, duration, false)
        }
        (Value::Duration(_), _) | (_, Value::Duration(_)) => Err(QueryError::new(
            QueryErrorKind::Type,
            "duration arithmetic received an incompatible operand",
        )),
        _ => return None,
    };
    Some(result)
}

fn combine_duration(
    left: &DurationValue,
    right: &DurationValue,
    subtract: bool,
) -> QueryResult<DurationValue> {
    let (lm, ld, ls, ln) = left.components();
    let (mut rm, mut rd, mut rs, mut rn) = right.components();
    if subtract {
        rm = checked_negate(rm)?;
        rd = checked_negate(rd)?;
        rs = checked_negate(rs)?;
        rn = checked_negate(rn)?;
    }
    duration_from_i128(
        i128::from(lm) + i128::from(rm),
        i128::from(ld) + i128::from(rd),
        i128::from(ls) + i128::from(rs),
        i128::from(ln) + i128::from(rn),
    )
}

fn scale_duration(duration: &DurationValue, scale: f64) -> QueryResult<DurationValue> {
    if !scale.is_finite() {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "duration scale must be finite",
        ));
    }
    let (months, days, seconds, nanoseconds) = duration.components();
    scaled_duration_from_components(months, days, seconds, nanoseconds, scale)
}

fn scaled_duration_from_components(
    months: i64,
    days: i64,
    seconds: i64,
    nanoseconds: i64,
    scale: f64,
) -> QueryResult<DurationValue> {
    let months = months as f64 * scale;
    let whole_months = months.trunc();
    let days = days as f64 * scale + (months - whole_months) * (48_699.0 / 1_600.0);
    let whole_days = days.trunc();
    let seconds = seconds as f64 * scale + (days - whole_days) * 86_400.0;
    let mut whole_seconds = seconds.trunc();
    let mut nanoseconds =
        (seconds - whole_seconds) * NANOS_PER_SECOND as f64 + nanoseconds as f64 * scale;
    let carry_seconds = (nanoseconds / NANOS_PER_SECOND as f64).trunc();
    whole_seconds += carry_seconds;
    nanoseconds -= carry_seconds * NANOS_PER_SECOND as f64;
    let rounded_nanoseconds = nanoseconds.round();
    let nanoseconds = if (nanoseconds - rounded_nanoseconds).abs() < 1e-6 {
        rounded_nanoseconds
    } else {
        nanoseconds.trunc()
    };
    for value in [whole_months, whole_days, whole_seconds, nanoseconds] {
        if value < i64::MIN as f64 || value > i64::MAX as f64 {
            return Err(duration_overflow());
        }
    }
    Ok(DurationValue::from_components(
        whole_months as i64,
        whole_days as i64,
        whole_seconds as i64,
        nanoseconds as i64,
    ))
}

fn divide_duration(duration: &DurationValue, divisor: f64) -> QueryResult<DurationValue> {
    if divisor == 0.0 || !divisor.is_finite() {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "duration divisor must be finite and non-zero",
        ));
    }
    scale_duration(duration, 1.0 / divisor)
}

fn duration_from_i128(
    months: i128,
    days: i128,
    seconds: i128,
    nanoseconds: i128,
) -> QueryResult<DurationValue> {
    let seconds = seconds
        .checked_add(nanoseconds.div_euclid(NANOS_PER_SECOND))
        .ok_or_else(duration_overflow)?;
    let nanoseconds = nanoseconds.rem_euclid(NANOS_PER_SECOND);
    Ok(DurationValue::from_components(
        i64::try_from(months).map_err(|_| duration_overflow())?,
        i64::try_from(days).map_err(|_| duration_overflow())?,
        i64::try_from(seconds).map_err(|_| duration_overflow())?,
        i64::try_from(nanoseconds).map_err(|_| duration_overflow())?,
    ))
}

fn checked_negate(value: i64) -> QueryResult<i64> {
    value.checked_neg().ok_or_else(duration_overflow)
}

fn duration_overflow() -> QueryError {
    QueryError::new(QueryErrorKind::Type, "duration arithmetic overflow")
}

fn shift_temporal(
    temporal: &Value,
    duration: &DurationValue,
    subtract: bool,
) -> QueryResult<Value> {
    let (mut months, mut days, mut seconds, mut nanoseconds) = duration.components();
    if subtract {
        months = checked_negate(months)?;
        days = checked_negate(days)?;
        seconds = checked_negate(seconds)?;
        nanoseconds = checked_negate(nanoseconds)?;
    }
    match temporal {
        Value::Date(value) => {
            shift_date(value, months, days, seconds, nanoseconds).map(Value::Date)
        }
        Value::LocalTime(value) => {
            shift_local_time(value, seconds, nanoseconds).map(Value::LocalTime)
        }
        Value::Time(value) => shift_time(value, seconds, nanoseconds).map(Value::Time),
        Value::LocalDateTime(value) => {
            shift_local_datetime(value, months, days, seconds, nanoseconds)
                .map(Value::LocalDateTime)
        }
        Value::ZonedDateTime(value) => {
            shift_zoned_datetime(value, months, days, seconds, nanoseconds)
                .map(Value::ZonedDateTime)
        }
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "duration can only be added to a temporal instant",
        )),
    }
}

fn shift_date(
    value: &DateValue,
    months: i64,
    days: i64,
    seconds: i64,
    nanoseconds: i64,
) -> QueryResult<DateValue> {
    let shifted = shift_days_by_months(value.days(), months)?;
    let time_nanoseconds = i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds);
    let time_days = time_nanoseconds / NANOS_PER_DAY;
    let days = i128::from(days)
        .checked_add(time_days)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "date arithmetic overflow"))?;
    let shifted = shifted
        .checked_add(days)
        .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "date arithmetic overflow"))?;
    DateValue::from_days(shifted).map_err(Into::into)
}

fn shift_days_by_months(days: i64, months: i64) -> QueryResult<i64> {
    let (year, month, day) = crate::cypher::civil_from_days(days);
    let total = i128::from(year)
        .checked_mul(12)
        .and_then(|value| value.checked_add(i128::from(month) - 1))
        .and_then(|value| value.checked_add(i128::from(months)))
        .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "date arithmetic overflow"))?;
    let year = i64::try_from(total.div_euclid(12))
        .map_err(|_| QueryError::new(QueryErrorKind::Type, "date arithmetic overflow"))?;
    let month = (total.rem_euclid(12) + 1) as u32;
    let day = day.min(crate::cypher::days_in_month(year, month));
    DateValue::from_components(year, month, day)
        .map(|value| value.days())
        .map_err(Into::into)
}

fn shift_local_time(
    value: &LocalTimeValue,
    seconds: i64,
    nanoseconds: i64,
) -> QueryResult<LocalTimeValue> {
    let delta = i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds);
    let result = (i128::from(value.nanoseconds()) + delta).rem_euclid(NANOS_PER_DAY);
    LocalTimeValue::from_nanoseconds(result as u64).map_err(Into::into)
}

fn shift_time(value: &TimeValue, seconds: i64, nanoseconds: i64) -> QueryResult<TimeValue> {
    let (local, offset) = value.storage_components();
    let local = shift_clock_nanoseconds(local, seconds, nanoseconds);
    TimeValue::from_components(local, offset).map_err(Into::into)
}

fn shift_clock_nanoseconds(local: u64, seconds: i64, nanoseconds: i64) -> u64 {
    let delta = i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds);
    (i128::from(local) + delta).rem_euclid(NANOS_PER_DAY) as u64
}

fn shift_local_datetime(
    value: &LocalDateTimeValue,
    months: i64,
    days: i64,
    seconds: i64,
    nanoseconds: i64,
) -> QueryResult<LocalDateTimeValue> {
    let (date, time) = value.storage_components();
    let date = shift_days_by_months(date, months)?
        .checked_add(days)
        .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "datetime arithmetic overflow"))?;
    let total = i128::from(date) * NANOS_PER_DAY
        + i128::from(time)
        + i128::from(seconds) * NANOS_PER_SECOND
        + i128::from(nanoseconds);
    let days = i64::try_from(total.div_euclid(NANOS_PER_DAY))
        .map_err(|_| QueryError::new(QueryErrorKind::Type, "datetime arithmetic overflow"))?;
    let time = total.rem_euclid(NANOS_PER_DAY) as u64;
    LocalDateTimeValue::from_components(days, time).map_err(Into::into)
}

fn shift_zoned_datetime(
    value: &ZonedDateTimeValue,
    months: i64,
    days: i64,
    seconds: i64,
    nanoseconds: i64,
) -> QueryResult<ZonedDateTimeValue> {
    let (date, time) = value.local_components()?;
    let date = shift_days_by_months(date, months)?
        .checked_add(days)
        .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "datetime arithmetic overflow"))?;
    let calendar_shifted =
        ZonedDateTimeValue::from_local(date, time, value.zone(), Some(value.offset_seconds()))?;
    let instant = calendar_shifted
        .instant_nanoseconds()
        .checked_add(i128::from(seconds) * NANOS_PER_SECOND + i128::from(nanoseconds))
        .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "datetime arithmetic overflow"))?;
    ZonedDateTimeValue::from_instant(instant, value.zone()).map_err(Into::into)
}

#[derive(Clone, Copy)]
enum TemporalFamily {
    Date,
    LocalTime,
    Time,
    LocalDateTime,
    ZonedDateTime,
}

fn truncate(values: &[Value], family: TemporalFamily) -> QueryResult<Value> {
    require_arity(values, 1, 3)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let Value::String(unit) = &values[0] else {
        return temporal_type_error("truncate unit must be String");
    };
    let input = match values.get(1) {
        Some(value) => truncate_input(value, family, unit)?,
        None => current_temporal(family)?,
    };
    let truncated = truncate_value(&input, family, unit)?;
    match values.get(2) {
        None => Ok(truncated),
        Some(Value::Map(fields)) => apply_truncate_fields(truncated, family, unit, fields),
        Some(_) => temporal_type_error("truncate fields must be Map"),
    }
}

fn current_temporal(family: TemporalFamily) -> QueryResult<Value> {
    let name = match family {
        TemporalFamily::Date => "date",
        TemporalFamily::LocalTime => "localtime",
        TemporalFamily::Time => "time",
        TemporalFamily::LocalDateTime => "localdatetime",
        TemporalFamily::ZonedDateTime => "datetime",
    };
    super::instant::evaluate(name, &[])
        .ok_or_else(|| QueryError::internal("temporal constructor is not registered"))?
}

fn truncate_input(value: &Value, family: TemporalFamily, unit: &str) -> QueryResult<Value> {
    match family {
        TemporalFamily::Date => truncate_date_input(value),
        TemporalFamily::LocalTime => truncate_local_time_input(value),
        TemporalFamily::Time => truncate_time_input(value),
        TemporalFamily::LocalDateTime => truncate_local_datetime_input(value, unit),
        TemporalFamily::ZonedDateTime => truncate_zoned_datetime_input(value, unit),
    }
}

fn truncate_date_input(value: &Value) -> QueryResult<Value> {
    match value {
        Value::Date(value) => Ok(Value::Date(value.clone())),
        Value::LocalDateTime(value) => Ok(Value::Date(DateValue::from_days(
            value.storage_components().0,
        )?)),
        Value::ZonedDateTime(value) => Ok(Value::Date(DateValue::from_days(
            value.local_components()?.0,
        )?)),
        _ => temporal_type_error("truncate input has the wrong temporal type"),
    }
}

fn truncate_local_time_input(value: &Value) -> QueryResult<Value> {
    let nanoseconds = match value {
        Value::LocalTime(value) => return Ok(Value::LocalTime(value.clone())),
        Value::Time(value) => value.storage_components().0,
        Value::LocalDateTime(value) => value.storage_components().1,
        Value::ZonedDateTime(value) => value.local_components()?.1,
        Value::Date(_) => 0,
        _ => return temporal_type_error("truncate input has the wrong temporal type"),
    };
    Ok(Value::LocalTime(LocalTimeValue::from_nanoseconds(
        nanoseconds,
    )?))
}

fn truncate_time_input(value: &Value) -> QueryResult<Value> {
    let (nanoseconds, offset) = match value {
        Value::Time(value) => return Ok(Value::Time(value.clone())),
        Value::LocalTime(value) => (value.nanoseconds(), 0),
        Value::LocalDateTime(value) => (value.storage_components().1, 0),
        Value::ZonedDateTime(value) => (value.local_components()?.1, value.offset_seconds()),
        Value::Date(_) => (0, 0),
        _ => return temporal_type_error("truncate input has the wrong temporal type"),
    };
    Ok(Value::Time(TimeValue::from_components(
        nanoseconds,
        offset,
    )?))
}

fn truncate_local_datetime_input(value: &Value, unit: &str) -> QueryResult<Value> {
    let result = match value {
        Value::LocalDateTime(value) => value.clone(),
        Value::ZonedDateTime(value) => {
            let (days, time) = value.local_components()?;
            LocalDateTimeValue::from_components(days, time)?
        }
        Value::Date(value) => {
            reject_date_subday_truncation(unit)?;
            LocalDateTimeValue::from_components(value.days(), 0)?
        }
        _ => return temporal_type_error("truncate input has the wrong temporal type"),
    };
    Ok(Value::LocalDateTime(result))
}

fn truncate_zoned_datetime_input(value: &Value, unit: &str) -> QueryResult<Value> {
    let result = match value {
        Value::ZonedDateTime(value) => value.clone(),
        Value::LocalDateTime(value) => {
            let (days, time) = value.storage_components();
            ZonedDateTimeValue::from_local(days, time, "Z", None)?
        }
        Value::Date(value) => {
            reject_date_subday_truncation(unit)?;
            ZonedDateTimeValue::from_local(value.days(), 0, "Z", None)?
        }
        _ => return temporal_type_error("truncate input has the wrong temporal type"),
    };
    Ok(Value::ZonedDateTime(result))
}

fn reject_date_subday_truncation(unit: &str) -> QueryResult<()> {
    if truncate_rank(unit).is_some_and(|rank| rank > 8) {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "DATE input cannot be truncated using a time unit smaller than day",
        ));
    }
    Ok(())
}

fn truncate_value(value: &Value, family: TemporalFamily, unit: &str) -> QueryResult<Value> {
    match (family, value) {
        (TemporalFamily::Date, Value::Date(value)) => truncate_date(value, unit).map(Value::Date),
        (TemporalFamily::LocalTime, Value::LocalTime(value)) => {
            truncate_local_time(value, unit).map(Value::LocalTime)
        }
        (TemporalFamily::Time, Value::Time(value)) => truncate_time(value, unit).map(Value::Time),
        (TemporalFamily::LocalDateTime, Value::LocalDateTime(value)) => {
            truncate_local_datetime(value, unit).map(Value::LocalDateTime)
        }
        (TemporalFamily::ZonedDateTime, Value::ZonedDateTime(value)) => {
            truncate_zoned_datetime(value, unit).map(Value::ZonedDateTime)
        }
        _ => Err(QueryError::internal("truncate input conversion failed")),
    }
}

fn apply_truncate_fields(
    truncated: Value,
    family: TemporalFamily,
    unit: &str,
    fields: &BTreeMap<String, Value>,
) -> QueryResult<Value> {
    validate_truncate_fields(family, unit, fields)?;
    let mut constructor_fields = fields.clone();
    preserve_truncated_fraction(&truncated, unit, &mut constructor_fields)?;
    let (name, base_key) = match family {
        TemporalFamily::Date => ("date", "date"),
        TemporalFamily::LocalTime => ("localtime", "time"),
        TemporalFamily::Time => ("time", "time"),
        TemporalFamily::LocalDateTime => ("localdatetime", "datetime"),
        TemporalFamily::ZonedDateTime => ("datetime", "datetime"),
    };
    let constructor_base = truncate_constructor_base(&truncated, family, &constructor_fields)?;
    constructor_fields.insert(base_key.to_owned(), constructor_base);
    let truncated = constructor_fields
        .get(base_key)
        .cloned()
        .ok_or_else(|| QueryError::internal("truncate base field is missing"))?;
    adjust_relative_date_fields(&truncated, unit, &mut constructor_fields)?;
    super::instant::evaluate(name, &[Value::Map(constructor_fields)])
        .ok_or_else(|| QueryError::internal("temporal constructor is not registered"))?
}

fn preserve_truncated_fraction(
    truncated: &Value,
    unit: &str,
    fields: &mut BTreeMap<String, Value>,
) -> QueryResult<()> {
    if !fields.contains_key("microsecond") && !fields.contains_key("nanosecond") {
        return Ok(());
    }
    let nanoseconds = match truncated {
        Value::LocalTime(value) => value.nanoseconds(),
        Value::Time(value) => value.storage_components().0,
        Value::LocalDateTime(value) => value.storage_components().1,
        Value::ZonedDateTime(value) => value.local_components()?.1,
        _ => return Ok(()),
    } % NANOS_PER_SECOND as u64;
    match unit.to_ascii_lowercase().as_str() {
        "millisecond" => {
            fields.insert(
                "millisecond".to_owned(),
                Value::Integer((nanoseconds / 1_000_000) as i64),
            );
        }
        "microsecond" => {
            fields.insert(
                "millisecond".to_owned(),
                Value::Integer((nanoseconds / 1_000_000) as i64),
            );
            fields.insert(
                "microsecond".to_owned(),
                Value::Integer((nanoseconds / 1_000 % 1_000) as i64),
            );
        }
        _ => {}
    }
    Ok(())
}

fn truncate_constructor_base(
    truncated: &Value,
    family: TemporalFamily,
    fields: &BTreeMap<String, Value>,
) -> QueryResult<Value> {
    if !fields.contains_key("timezone") {
        return Ok(truncated.clone());
    }
    match (family, truncated) {
        (TemporalFamily::Time, Value::Time(value)) => Ok(Value::LocalTime(
            LocalTimeValue::from_nanoseconds(value.storage_components().0)?,
        )),
        (TemporalFamily::ZonedDateTime, Value::ZonedDateTime(value)) => {
            let (days, time) = value.local_components()?;
            Ok(Value::LocalDateTime(LocalDateTimeValue::from_components(
                days, time,
            )?))
        }
        _ => Ok(truncated.clone()),
    }
}

fn validate_truncate_fields(
    family: TemporalFamily,
    unit: &str,
    fields: &BTreeMap<String, Value>,
) -> QueryResult<()> {
    let unit_rank = truncate_rank(unit)
        .ok_or_else(|| QueryError::semantic("unsupported temporal truncation unit"))?;
    for key in fields.keys() {
        if key == "timezone" {
            if matches!(family, TemporalFamily::Time | TemporalFamily::ZonedDateTime) {
                continue;
            }
            return Err(QueryError::semantic(
                "timezone is not available for this truncate function",
            ));
        }
        let (field_family, field_rank) = truncate_field(key)
            .ok_or_else(|| QueryError::semantic(format!("unsupported truncate field {key}")))?;
        let compatible = match family {
            TemporalFamily::Date => field_family == TruncateFieldFamily::Date,
            TemporalFamily::LocalTime | TemporalFamily::Time => {
                field_family == TruncateFieldFamily::Time
            }
            TemporalFamily::LocalDateTime | TemporalFamily::ZonedDateTime => true,
        };
        if !compatible {
            return Err(QueryError::semantic(format!(
                "truncate field {key} is not available for this temporal type"
            )));
        }
        if field_rank <= unit_rank {
            return Err(QueryError::semantic(format!(
                "truncate field {key} must be smaller than unit {unit}"
            )));
        }
    }
    Ok(())
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum TruncateFieldFamily {
    Date,
    Time,
}

fn truncate_rank(name: &str) -> Option<u8> {
    match name.to_ascii_lowercase().as_str() {
        "millennium" => Some(0),
        "century" => Some(1),
        "decade" => Some(2),
        "year" | "weekyear" => Some(3),
        "quarter" => Some(4),
        "month" | "week" => Some(5),
        "day" => Some(8),
        "hour" => Some(9),
        "minute" => Some(10),
        "second" => Some(11),
        "millisecond" => Some(12),
        "microsecond" => Some(13),
        "nanosecond" => Some(14),
        _ => None,
    }
}

fn truncate_field(name: &str) -> Option<(TruncateFieldFamily, u8)> {
    let family = match name {
        "year" | "weekYear" | "quarter" | "month" | "week" | "day" | "dayOfWeek"
        | "dayOfQuarter" | "ordinalDay" => TruncateFieldFamily::Date,
        "hour" | "minute" | "second" | "millisecond" | "microsecond" | "nanosecond" => {
            TruncateFieldFamily::Time
        }
        _ => return None,
    };
    let rank = match name {
        "year" | "weekYear" => 3,
        "quarter" => 4,
        "month" | "week" => 5,
        "day" | "dayOfWeek" | "dayOfQuarter" | "ordinalDay" => 8,
        value => truncate_rank(value)?,
    };
    Some((family, rank))
}

fn adjust_relative_date_fields(
    truncated: &Value,
    unit: &str,
    fields: &mut BTreeMap<String, Value>,
) -> QueryResult<()> {
    let days = match truncated {
        Value::Date(value) => Some(value.days()),
        Value::LocalDateTime(value) => Some(value.storage_components().0),
        Value::ZonedDateTime(value) => Some(value.local_components()?.0),
        _ => None,
    };
    let Some(days) = days else {
        return Ok(());
    };
    let unit = unit.to_ascii_lowercase();
    match unit.as_str() {
        "week" => adjust_day_of_week(truncated, days, fields),
        "quarter" => adjust_day_of_quarter(truncated, days, fields),
        _ => Ok(()),
    }
}

fn adjust_day_of_week(
    truncated: &Value,
    days: i64,
    fields: &mut BTreeMap<String, Value>,
) -> QueryResult<()> {
    if fields.contains_key("week") {
        return Ok(());
    }
    let Some(day) = take_integer_field(fields, "dayOfWeek")? else {
        return Ok(());
    };
    if !(1..=7).contains(&day) {
        return temporal_type_error("dayOfWeek must be between 1 and 7");
    }
    replace_truncate_date(truncated, days + day - 1, fields)
}

fn adjust_day_of_quarter(
    truncated: &Value,
    days: i64,
    fields: &mut BTreeMap<String, Value>,
) -> QueryResult<()> {
    if fields.contains_key("quarter") {
        return Ok(());
    }
    let Some(day) = take_integer_field(fields, "dayOfQuarter")? else {
        return Ok(());
    };
    if day < 1 {
        return temporal_type_error("dayOfQuarter must be positive");
    }
    let candidate = days
        .checked_add(day - 1)
        .ok_or_else(|| QueryError::semantic("truncate date field overflow"))?;
    let (year, month, _) = crate::cypher::civil_from_days(days);
    let next_month = (month - 1) / 3 * 3 + 4;
    let end = if next_month > 12 {
        date_days(year + 1, 1, 1)?
    } else {
        date_days(year, next_month, 1)?
    };
    if candidate >= end {
        return temporal_type_error("dayOfQuarter is outside the selected quarter");
    }
    replace_truncate_date(truncated, candidate, fields)
}

fn take_integer_field(
    fields: &mut BTreeMap<String, Value>,
    name: &str,
) -> QueryResult<Option<i64>> {
    match fields.remove(name) {
        None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(value)),
        Some(_) => temporal_type_error(&format!("truncate field {name} must be Integer")),
    }
}

fn replace_truncate_date(
    truncated: &Value,
    days: i64,
    fields: &mut BTreeMap<String, Value>,
) -> QueryResult<()> {
    match truncated {
        Value::Date(_) => {
            fields.insert("date".to_owned(), Value::Date(DateValue::from_days(days)?));
        }
        Value::LocalDateTime(value) => {
            fields.insert(
                "datetime".to_owned(),
                Value::LocalDateTime(LocalDateTimeValue::from_components(
                    days,
                    value.storage_components().1,
                )?),
            );
        }
        Value::ZonedDateTime(value) => {
            fields.insert(
                "datetime".to_owned(),
                Value::ZonedDateTime(ZonedDateTimeValue::from_local(
                    days,
                    value.local_components()?.1,
                    value.zone(),
                    Some(value.offset_seconds()),
                )?),
            );
        }
        _ => return Err(QueryError::internal("truncate value has no date")),
    }
    Ok(())
}

fn truncate_date(value: &DateValue, unit: &str) -> QueryResult<DateValue> {
    let days = truncate_date_days(value.days(), unit)?;
    DateValue::from_days(days).map_err(Into::into)
}

fn truncate_date_days(days: i64, unit: &str) -> QueryResult<i64> {
    let (year, month, _) = crate::cypher::civil_from_days(days);
    match unit.to_ascii_lowercase().as_str() {
        "millennium" => date_days(year.div_euclid(1000) * 1000, 1, 1),
        "century" => date_days(year.div_euclid(100) * 100, 1, 1),
        "decade" => date_days(year.div_euclid(10) * 10, 1, 1),
        "year" => date_days(year, 1, 1),
        "weekyear" => {
            let day_of_week = (days + 3).rem_euclid(7) + 1;
            let week_year = crate::cypher::civil_from_days(days + (4 - day_of_week)).0;
            let january_fourth = date_days(week_year, 1, 4)?;
            Ok(start_of_week(january_fourth))
        }
        "quarter" => date_days(year, (month - 1) / 3 * 3 + 1, 1),
        "month" => date_days(year, month, 1),
        "week" => Ok(start_of_week(days)),
        "day" => Ok(days),
        _ => Err(QueryError::semantic("unsupported date truncation unit")),
    }
}

fn date_days(year: i64, month: u32, day: u32) -> QueryResult<i64> {
    DateValue::from_components(year, month, day)
        .map(|value| value.days())
        .map_err(Into::into)
}

fn start_of_week(days: i64) -> i64 {
    days - (days + 3).rem_euclid(7)
}

fn truncate_local_time(value: &LocalTimeValue, unit: &str) -> QueryResult<LocalTimeValue> {
    LocalTimeValue::from_nanoseconds(truncate_time_nanos(value.nanoseconds(), unit)?)
        .map_err(Into::into)
}

fn truncate_time(value: &TimeValue, unit: &str) -> QueryResult<TimeValue> {
    let (local, offset) = value.storage_components();
    TimeValue::from_components(truncate_time_nanos(local, unit)?, offset).map_err(Into::into)
}

fn truncate_time_nanos(value: u64, unit: &str) -> QueryResult<u64> {
    let quantum = match unit.to_ascii_lowercase().as_str() {
        "day" => NANOS_PER_DAY,
        "hour" => 3_600 * NANOS_PER_SECOND,
        "minute" => 60 * NANOS_PER_SECOND,
        "second" => NANOS_PER_SECOND,
        "millisecond" => 1_000_000,
        "microsecond" => 1_000,
        "nanosecond" => 1,
        _ => return Err(QueryError::semantic("unsupported time truncation unit")),
    };
    Ok((i128::from(value) / quantum * quantum) as u64)
}

fn truncate_local_datetime(
    value: &LocalDateTimeValue,
    unit: &str,
) -> QueryResult<LocalDateTimeValue> {
    let (days, time) = value.storage_components();
    let (days, time) = truncate_datetime_parts(days, time, unit)?;
    LocalDateTimeValue::from_components(days, time).map_err(Into::into)
}

fn truncate_zoned_datetime(
    value: &ZonedDateTimeValue,
    unit: &str,
) -> QueryResult<ZonedDateTimeValue> {
    let (days, time) = value.local_components()?;
    let (days, time) = truncate_datetime_parts(days, time, unit)?;
    ZonedDateTimeValue::from_local(days, time, value.zone(), Some(value.offset_seconds()))
        .map_err(Into::into)
}

fn truncate_datetime_parts(days: i64, time: u64, unit: &str) -> QueryResult<(i64, u64)> {
    match unit.to_ascii_lowercase().as_str() {
        "millennium" | "century" | "decade" | "year" | "weekyear" | "quarter" | "month"
        | "week" | "day" => Ok((truncate_date_days(days, unit)?, 0)),
        _ => Ok((days, truncate_time_nanos(time, unit)?)),
    }
}

fn datetime_from_epoch(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let seconds = numeric_epoch_component(&values[0], "seconds")?;
    let nanoseconds = numeric_epoch_component(&values[1], "nanoseconds")?;
    let instant = seconds
        .checked_mul(NANOS_PER_SECOND)
        .and_then(|value| value.checked_add(nanoseconds))
        .ok_or_else(epoch_overflow)?;
    ZonedDateTimeValue::from_instant(instant, "Z")
        .map(Value::ZonedDateTime)
        .map_err(Into::into)
}

fn datetime_from_epoch_millis(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    if matches!(values[0], Value::Null) {
        return Ok(Value::Null);
    }
    let milliseconds = numeric_epoch_component(&values[0], "milliseconds")?;
    let instant = milliseconds
        .checked_mul(1_000_000)
        .ok_or_else(epoch_overflow)?;
    ZonedDateTimeValue::from_instant(instant, "Z")
        .map(Value::ZonedDateTime)
        .map_err(Into::into)
}

fn numeric_epoch_component(value: &Value, name: &str) -> QueryResult<i128> {
    match value {
        Value::Integer(value) => Ok(i128::from(*value)),
        Value::Float(value)
            if value.is_finite() && *value >= i64::MIN as f64 && *value <= i64::MAX as f64 =>
        {
            Ok(value.trunc() as i128)
        }
        _ => temporal_type_error(&format!("epoch {name} must be numeric")),
    }
}

fn epoch_overflow() -> QueryError {
    QueryError::new(
        QueryErrorKind::Type,
        "epoch value is outside the temporal range",
    )
}

#[derive(Clone, Copy)]
enum Difference {
    Logical,
    Months,
    Days,
    Seconds,
}

fn duration_difference(values: &[Value], mode: Difference) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let from = temporal_point(&values[0], Some(&values[1]))?;
    let to = temporal_point(&values[1], Some(&values[0]))?;
    let duration = match mode {
        Difference::Seconds => difference_in_seconds(from, to)?,
        Difference::Days => difference_in_days(from, to)?,
        Difference::Months => difference_in_months(from, to)?,
        Difference::Logical => difference_logically(from, to)?,
    };
    Ok(Value::Duration(duration))
}

#[derive(Clone, Copy)]
struct TemporalPoint {
    days: i64,
    nanoseconds: u64,
    instant_nanoseconds: Option<i128>,
    offset_seconds: Option<i32>,
    has_date: bool,
}

fn temporal_point(value: &Value, counterpart: Option<&Value>) -> QueryResult<TemporalPoint> {
    let counterpart_days = counterpart.and_then(temporal_days).unwrap_or(0);
    let counterpart_zone = counterpart.and_then(|value| match value {
        Value::ZonedDateTime(value) => Some(value.zone()),
        _ => None,
    });
    match value {
        Value::Date(value) => temporal_point_local(value.days(), 0, true, counterpart_zone),
        Value::LocalTime(value) => temporal_point_local(
            counterpart_days,
            value.nanoseconds(),
            false,
            counterpart_zone,
        ),
        Value::Time(value) => {
            let (local, offset) = value.storage_components();
            Ok(TemporalPoint {
                days: counterpart_days,
                nanoseconds: local,
                instant_nanoseconds: Some(
                    i128::from(counterpart_days) * NANOS_PER_DAY + value.instant_key(),
                ),
                offset_seconds: Some(offset),
                has_date: false,
            })
        }
        Value::LocalDateTime(value) => {
            let (days, nanoseconds) = value.storage_components();
            temporal_point_local(days, nanoseconds, true, counterpart_zone)
        }
        Value::ZonedDateTime(value) => {
            let (days, nanoseconds) = value.local_components()?;
            Ok(TemporalPoint {
                days,
                nanoseconds,
                instant_nanoseconds: Some(value.instant_nanoseconds()),
                offset_seconds: Some(value.offset_seconds()),
                has_date: true,
            })
        }
        _ => temporal_type_error("duration difference requires temporal instant arguments"),
    }
}

fn temporal_point_local(
    days: i64,
    nanoseconds: u64,
    has_date: bool,
    counterpart_zone: Option<&str>,
) -> QueryResult<TemporalPoint> {
    let (instant_nanoseconds, offset_seconds) = match counterpart_zone {
        Some(zone) => {
            let zoned = ZonedDateTimeValue::from_local(days, nanoseconds, zone, None)?;
            (
                Some(zoned.instant_nanoseconds()),
                Some(zoned.offset_seconds()),
            )
        }
        None => (None, None),
    };
    Ok(TemporalPoint {
        days,
        nanoseconds,
        instant_nanoseconds,
        offset_seconds,
        has_date,
    })
}

fn temporal_days(value: &Value) -> Option<i64> {
    match value {
        Value::Date(value) => Some(value.days()),
        Value::LocalDateTime(value) => Some(value.storage_components().0),
        Value::ZonedDateTime(value) => value.local_components().ok().map(|parts| parts.0),
        _ => None,
    }
}

fn elapsed_nanoseconds(from: TemporalPoint, to: TemporalPoint) -> i128 {
    match (from.instant_nanoseconds, to.instant_nanoseconds) {
        (Some(from), Some(to)) => to - from,
        _ => {
            (i128::from(to.days) - i128::from(from.days)) * NANOS_PER_DAY
                + i128::from(to.nanoseconds)
                - i128::from(from.nanoseconds)
        }
    }
}

fn difference_in_seconds(from: TemporalPoint, to: TemporalPoint) -> QueryResult<DurationValue> {
    let elapsed = elapsed_nanoseconds(from, to);
    duration_from_i128(0, 0, elapsed / NANOS_PER_SECOND, elapsed % NANOS_PER_SECOND)
}

fn difference_in_days(from: TemporalPoint, to: TemporalPoint) -> QueryResult<DurationValue> {
    let elapsed = elapsed_nanoseconds(from, to);
    duration_from_i128(0, elapsed / NANOS_PER_DAY, 0, 0)
}

fn difference_in_months(from: TemporalPoint, to: TemporalPoint) -> QueryResult<DurationValue> {
    if !from.has_date || !to.has_date {
        return Ok(DurationValue::from_components(0, 0, 0, 0));
    }
    let months = whole_months_between(from, to)?;
    duration_from_i128(i128::from(months), 0, 0, 0)
}

fn difference_logically(from: TemporalPoint, to: TemporalPoint) -> QueryResult<DurationValue> {
    if !from.has_date || !to.has_date {
        return difference_in_seconds(from, to);
    }
    let months = whole_months_between(from, to)?;
    let shifted_days = shift_days_by_months(from.days, months)?;
    let shifted_local = i128::from(shifted_days) * NANOS_PER_DAY + i128::from(from.nanoseconds);
    let target_local = i128::from(to.days) * NANOS_PER_DAY + i128::from(to.nanoseconds);
    let remainder = match (from.offset_seconds, to.instant_nanoseconds) {
        (Some(offset), Some(target)) => {
            target - (shifted_local - i128::from(offset) * NANOS_PER_SECOND)
        }
        _ => target_local - shifted_local,
    };
    duration_from_i128(
        i128::from(months),
        remainder / NANOS_PER_DAY,
        (remainder % NANOS_PER_DAY) / NANOS_PER_SECOND,
        remainder % NANOS_PER_SECOND,
    )
}

fn whole_months_between(from: TemporalPoint, to: TemporalPoint) -> QueryResult<i64> {
    let (from_year, from_month, _) = crate::cypher::civil_from_days(from.days);
    let (to_year, to_month, _) = crate::cypher::civil_from_days(to.days);
    let raw = i128::from(to_year - from_year) * 12 + i128::from(to_month) - i128::from(from_month);
    let mut months = i64::try_from(raw).map_err(|_| duration_overflow())?;
    let shifted_days = shift_days_by_months(from.days, months)?;
    let shifted_local = i128::from(shifted_days) * NANOS_PER_DAY + i128::from(from.nanoseconds);
    let shifted = from.offset_seconds.map_or(shifted_local, |offset| {
        shifted_local - i128::from(offset) * NANOS_PER_SECOND
    });
    let target_local = i128::from(to.days) * NANOS_PER_DAY + i128::from(to.nanoseconds);
    let target = to.instant_nanoseconds.unwrap_or(target_local);
    if months > 0 && shifted > target {
        months -= 1;
    } else if months < 0 && shifted < target {
        months += 1;
    }
    Ok(months)
}

fn format_value(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let pattern = match values.get(1) {
        None => None,
        Some(Value::String(pattern)) => Some(pattern.as_str()),
        Some(_) => return temporal_type_error("format pattern must be String"),
    };
    let output = match (&values[0], pattern) {
        (Value::Date(value), None) => value.as_str().to_owned(),
        (Value::LocalTime(value), None) => value.as_str().to_owned(),
        (Value::Time(value), None) => value.as_str().to_owned(),
        (Value::LocalDateTime(value), None) => value.as_str().to_owned(),
        (Value::ZonedDateTime(value), None)
            if value.zone() == "Z"
                || value.zone().starts_with('+')
                || value.zone().starts_with('-') =>
        {
            value.value().to_owned()
        }
        (Value::ZonedDateTime(value), None) => format!("{}[{}]", value.value(), value.zone()),
        (Value::Duration(value), None) => value.as_str().to_owned(),
        (Value::Duration(value), Some(pattern)) => {
            super::duration_format::format_duration(value, pattern)?
        }
        (value, Some(pattern)) => format_instant(value, pattern)?,
        _ => return temporal_type_error("format input must be a temporal value"),
    };
    Ok(Value::String(output))
}

#[derive(Clone, Copy)]
pub(super) struct InstantParts<'a> {
    pub(super) date: Option<(i64, u32, u32, i64)>,
    pub(super) time: Option<u64>,
    pub(super) offset: Option<i32>,
    pub(super) zone: Option<&'a str>,
}

fn instant_parts(value: &Value) -> QueryResult<InstantParts<'_>> {
    match value {
        Value::Date(value) => Ok(InstantParts {
            date: Some(date_parts(value.days())),
            time: None,
            offset: None,
            zone: None,
        }),
        Value::LocalTime(value) => Ok(InstantParts {
            date: None,
            time: Some(value.nanoseconds()),
            offset: None,
            zone: None,
        }),
        Value::Time(value) => Ok(InstantParts {
            date: None,
            time: Some(value.storage_components().0),
            offset: Some(value.offset_seconds()),
            zone: None,
        }),
        Value::LocalDateTime(value) => {
            let (days, time) = value.storage_components();
            Ok(InstantParts {
                date: Some(date_parts(days)),
                time: Some(time),
                offset: None,
                zone: None,
            })
        }
        Value::ZonedDateTime(value) => {
            let (days, time) = value.local_components()?;
            Ok(InstantParts {
                date: Some(date_parts(days)),
                time: Some(time),
                offset: Some(value.offset_seconds()),
                zone: Some(value.zone()),
            })
        }
        _ => temporal_type_error("format input must be a temporal instant"),
    }
}

fn date_parts(days: i64) -> (i64, u32, u32, i64) {
    let (year, month, day) = crate::cypher::civil_from_days(days);
    (year, month, day, days)
}

fn format_instant(value: &Value, pattern: &str) -> QueryResult<String> {
    let parts = instant_parts(value)?;
    render_pattern(pattern, |character, count| {
        format_instant_token(parts, character, count)
    })
}

fn format_instant_token(
    parts: InstantParts<'_>,
    character: char,
    count: usize,
) -> QueryResult<String> {
    let date = || {
        parts
            .date
            .ok_or_else(|| invalid_pattern_component(character))
    };
    let time = || {
        parts
            .time
            .ok_or_else(|| invalid_pattern_component(character))
    };
    let numeric = |value: i128| pad_number(value, count);
    Ok(match character {
        'G' => format_era(date()?.0, count)?,
        'u' => format_year(date()?.0, count),
        'y' => format_year(year_of_era(date()?.0), count),
        'Y' => format_year(iso_week_parts(date()?.3).0, count),
        'M' | 'L' if count <= 2 => numeric(i128::from(date()?.1)),
        'M' | 'L' if count == 3 => month_name(date()?.1, false).to_owned(),
        'M' | 'L' if count == 4 => month_name(date()?.1, true).to_owned(),
        'M' | 'L' if count == 5 => month_name(date()?.1, true)[..1].to_owned(),
        'M' | 'L' => return Err(invalid_pattern_component(character)),
        'd' => numeric(i128::from(date()?.2)),
        'D' => numeric(i128::from(day_of_year(date()?.3))),
        'g' => numeric(i128::from(date()?.3) + 2_440_588),
        'E' => format_weekday(date()?.3, count, false)?,
        'e' | 'c' => format_weekday(date()?.3, count, true)?,
        'F' | 'W' => numeric(i128::from((date()?.2 - 1) / 7 + 1)),
        'w' => numeric(i128::from(iso_week_parts(date()?.3).1)),
        'Q' | 'q' => format_quarter((date()?.1 - 1) / 3 + 1, count)?,
        'H' => numeric(i128::from(hour(time()?))),
        'k' => numeric(i128::from(if hour(time()?) == 0 {
            24
        } else {
            hour(time()?)
        })),
        'K' => numeric(i128::from(hour(time()?) % 12)),
        'h' => numeric(i128::from(match hour(time()?) % 12 {
            0 => 12,
            value => value,
        })),
        'm' => numeric(i128::from(minute(time()?))),
        's' => numeric(i128::from(second(time()?))),
        'S' => fraction(time()?, count)?,
        'n' => numeric(i128::from(time()? % 1_000_000_000)),
        'N' => numeric(i128::from(time()?)),
        'A' => numeric(i128::from(time()? / 1_000_000)),
        'a' => if hour(time()?) < 12 { "AM" } else { "PM" }.to_owned(),
        'B' => period_of_day(hour(time()?)).to_owned(),
        'O' => format_localized_offset(
            parts
                .offset
                .ok_or_else(|| invalid_pattern_component(character))?,
            count,
        )?,
        'X' | 'x' | 'Z' => format_pattern_offset(
            parts
                .offset
                .ok_or_else(|| invalid_pattern_component(character))?,
            character,
            count,
        )?,
        'V' if count == 2 => zone_id(parts)?,
        'V' => return Err(invalid_pattern_component(character)),
        'v' if matches!(count, 1 | 4) => zone_name(parts)?,
        'v' => return Err(invalid_pattern_component(character)),
        'z' if (1..=4).contains(&count) => zone_name(parts)?,
        'z' => return Err(invalid_pattern_component(character)),
        _ => return Err(invalid_pattern_component(character)),
    })
}

pub(super) fn render_pattern(
    pattern: &str,
    mut token: impl FnMut(char, usize) -> QueryResult<String>,
) -> QueryResult<String> {
    let characters = pattern.chars().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\'' {
            index = render_quoted(&characters, index, &mut output)?;
            continue;
        }
        if characters[index].is_ascii_alphabetic() {
            let character = characters[index];
            let start = index;
            while index < characters.len() && characters[index] == character {
                index += 1;
            }
            let count = index - start;
            if character == 'p' {
                let next = characters.get(index).copied().ok_or_else(|| {
                    QueryError::semantic("pad modifier requires a following pattern component")
                })?;
                if !next.is_ascii_alphabetic() || next == 'p' {
                    return Err(QueryError::semantic(
                        "pad modifier requires a following pattern component",
                    ));
                }
                let component_start = index;
                while index < characters.len() && characters[index] == next {
                    index += 1;
                }
                let rendered = token(next, index - component_start)?;
                if rendered.chars().count() > count {
                    return Err(QueryError::semantic(
                        "temporal component exceeds its requested pad width",
                    ));
                }
                output.extend(std::iter::repeat_n(' ', count - rendered.chars().count()));
                output.push_str(&rendered);
            } else {
                output.push_str(&token(character, count)?);
            }
            continue;
        }
        if matches!(characters[index], '[' | ']' | '{' | '}' | '#') {
            return Err(QueryError::semantic(
                "reserved temporal format character must be quoted",
            ));
        }
        output.push(characters[index]);
        index += 1;
    }
    Ok(output)
}

fn render_quoted(characters: &[char], start: usize, output: &mut String) -> QueryResult<usize> {
    if characters.get(start + 1) == Some(&'\'') {
        output.push('\'');
        return Ok(start + 2);
    }
    let mut index = start + 1;
    while index < characters.len() {
        if characters[index] == '\'' {
            if characters.get(index + 1) == Some(&'\'') {
                output.push('\'');
                index += 2;
                continue;
            }
            return Ok(index + 1);
        }
        output.push(characters[index]);
        index += 1;
    }
    Err(QueryError::semantic("unterminated temporal format literal"))
}

pub(super) fn pad_number(value: i128, width: usize) -> String {
    if width <= 1 {
        value.to_string()
    } else if value < 0 {
        format!("-{:0width$}", value.abs(), width = width)
    } else {
        format!("{value:0width$}", width = width)
    }
}

fn fraction(time: u64, width: usize) -> QueryResult<String> {
    if !(1..=9).contains(&width) {
        return Err(QueryError::semantic(
            "fraction-of-second format width must be between one and nine",
        ));
    }
    Ok(format!("{:09}", time % 1_000_000_000)[..width].to_owned())
}

fn format_pattern_offset(offset: i32, token: char, width: usize) -> QueryResult<String> {
    if offset == 0 && token == 'X' {
        return Ok("Z".to_owned());
    }
    let sign = if offset < 0 { '-' } else { '+' };
    let absolute = offset.unsigned_abs();
    let hour = absolute / 3_600;
    let minute = absolute % 3_600 / 60;
    let second = absolute % 60;
    Ok(match (token, width) {
        ('X' | 'x', 1) if minute == 0 && second == 0 => format!("{sign}{hour:02}"),
        ('X' | 'x', 1) => format!("{sign}{hour:02}{minute:02}"),
        ('X' | 'x', 2) | ('Z', 1..=3) => format!("{sign}{hour:02}{minute:02}"),
        ('X' | 'x', 3) => format!("{sign}{hour:02}:{minute:02}"),
        ('X' | 'x', 4) => format!("{sign}{hour:02}{minute:02}{second:02}"),
        ('X' | 'x', 5) => format!("{sign}{hour:02}:{minute:02}:{second:02}"),
        ('Z', 4) => format_localized_offset(offset, 4)?,
        ('Z', 5) if offset == 0 => "Z".to_owned(),
        ('Z', 5) if second == 0 => format!("{sign}{hour:02}:{minute:02}"),
        ('Z', 5) => format!("{sign}{hour:02}:{minute:02}:{second:02}"),
        _ => {
            return Err(QueryError::semantic(
                "unsupported timezone-offset format width",
            ));
        }
    })
}

fn day_of_year(days: i64) -> i64 {
    let (year, _, _) = crate::cypher::civil_from_days(days);
    days - crate::cypher::days_from_civil(year, 1, 1) + 1
}

fn hour(time: u64) -> u32 {
    (time / 3_600_000_000_000) as u32
}

fn minute(time: u64) -> u32 {
    (time / 60_000_000_000 % 60) as u32
}

fn second(time: u64) -> u32 {
    (time / 1_000_000_000 % 60) as u32
}

pub(super) fn invalid_pattern_component(character: char) -> QueryError {
    QueryError::semantic(format!(
        "temporal format component {character} is unavailable for this value"
    ))
}

fn temporal_type_error<T>(message: &str) -> QueryResult<T> {
    Err(QueryError::new(QueryErrorKind::Type, message))
}
