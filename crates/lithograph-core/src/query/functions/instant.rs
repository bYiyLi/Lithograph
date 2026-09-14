use std::cell::RefCell;

use chrono::{
    DateTime, Datelike, FixedOffset, NaiveDate, NaiveDateTime, NaiveTime, SecondsFormat, Timelike,
    Utc,
};

use super::require_arity;
use crate::cypher::{
    DateValue, LocalDateTimeValue, LocalTimeValue, TimeValue, Value, ZonedDateTimeValue,
};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

mod normalize;
mod zone;

use normalize::{
    local_datetime_text as normalize_local_datetime_text,
    local_time_text as normalize_local_time_text, offset_text as normalize_offset_text,
    zoned_time_text as normalize_zoned_time_text,
};
use zone::{normalize_time_with_default_zone, shifted_time_for_offset, zoned_datetime_from_map};

thread_local! {
    static STATEMENT_TIME: RefCell<Option<DateTime<Utc>>> = const { RefCell::new(None) };
    static TRANSACTION_TIME: RefCell<Option<DateTime<Utc>>> = const { RefCell::new(None) };
}

pub(crate) struct StatementClockGuard(Option<DateTime<Utc>>);

impl Drop for StatementClockGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        STATEMENT_TIME.with(|clock| *clock.borrow_mut() = previous);
    }
}

pub(crate) fn install_statement_time(value: DateTime<Utc>) -> StatementClockGuard {
    let previous = STATEMENT_TIME.with(|clock| clock.borrow_mut().replace(value));
    StatementClockGuard(previous)
}

pub(crate) struct TransactionClockGuard(Option<DateTime<Utc>>);

impl Drop for TransactionClockGuard {
    fn drop(&mut self) {
        let previous = self.0.take();
        TRANSACTION_TIME.with(|clock| *clock.borrow_mut() = previous);
    }
}

pub(crate) fn install_transaction_time(value: DateTime<Utc>) -> TransactionClockGuard {
    let previous = TRANSACTION_TIME.with(|clock| clock.borrow_mut().replace(value));
    TransactionClockGuard(previous)
}

fn statement_time() -> DateTime<Utc> {
    STATEMENT_TIME
        .with(|clock| *clock.borrow())
        .unwrap_or_else(Utc::now)
}

fn transaction_time() -> DateTime<Utc> {
    TRANSACTION_TIME
        .with(|clock| *clock.borrow())
        .unwrap_or_else(statement_time)
}

fn with_transaction_clock<T>(operation: impl FnOnce() -> T) -> T {
    let _guard = install_statement_time(transaction_time());
    operation()
}

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let result = match name {
        "date" => date(values, false, false),
        "date.realtime" => date(values, true, true),
        "date.statement" => date(values, false, true),
        "date.transaction" => with_transaction_clock(|| date(values, false, true)),
        "localtime" | "local_time" => local_time(values, false, false),
        "localtime.realtime" => local_time(values, true, true),
        "localtime.statement" => local_time(values, false, true),
        "localtime.transaction" => with_transaction_clock(|| local_time(values, false, true)),
        "time" | "zoned_time" => time(values, false, false),
        "time.realtime" => time(values, true, true),
        "time.statement" => time(values, false, true),
        "time.transaction" => with_transaction_clock(|| time(values, false, true)),
        "localdatetime" | "local_datetime" => local_datetime(values, false, false),
        "localdatetime.realtime" => local_datetime(values, true, true),
        "localdatetime.statement" => local_datetime(values, false, true),
        "localdatetime.transaction" => {
            with_transaction_clock(|| local_datetime(values, false, true))
        }
        "datetime" | "zoned_datetime" => zoned_datetime(values, false, false),
        "datetime.realtime" => zoned_datetime(values, true, true),
        "datetime.statement" => zoned_datetime(values, false, true),
        "datetime.transaction" => with_transaction_clock(|| zoned_datetime(values, false, true)),
        "timestamp" => timestamp(values),
        _ => return None,
    };
    Some(result)
}

fn date(values: &[Value], realtime: bool, clock_function: bool) -> QueryResult<Value> {
    require_arity(values, 0, if clock_function { 1 } else { 2 })?;
    if values.len() == 2 {
        return parse_temporal_with_pattern(&values[0], &values[1], TemporalTarget::Date);
    }
    let text = match values.first() {
        None => clock(realtime).format("%Y-%m-%d").to_string(),
        Some(Value::String(value)) => date_from_string(value, realtime, clock_function)?,
        Some(Value::Date(value)) => return Ok(Value::Date(value.clone())),
        Some(Value::LocalDateTime(value)) => DateValue::from_days(value.storage_components().0)?
            .as_str()
            .to_owned(),
        Some(Value::ZonedDateTime(value)) => DateValue::from_days(value.local_components()?.0)?
            .as_str()
            .to_owned(),
        Some(Value::Map(value)) => date_from_map_input(value, realtime)?,
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => return temporal_type_error("date"),
    };
    DateValue::parse(&text).map(Value::Date).map_err(Into::into)
}

fn date_from_string(value: &str, realtime: bool, clock_function: bool) -> QueryResult<String> {
    if clock_function {
        return Ok(current_parts(realtime, value)?.0);
    }
    normalize_date_text(value)
}

fn date_from_map_input(
    value: &std::collections::BTreeMap<String, Value>,
    realtime: bool,
) -> QueryResult<String> {
    if timezone_only(value) {
        return Ok(current_parts(realtime, timezone(value)?)?.0);
    }
    validate_temporal_map(value, TemporalMapTarget::Date)?;
    date_from_map(value)
}

fn local_time(values: &[Value], realtime: bool, clock_function: bool) -> QueryResult<Value> {
    require_arity(values, 0, if clock_function { 1 } else { 2 })?;
    if values.len() == 2 {
        return parse_temporal_with_pattern(&values[0], &values[1], TemporalTarget::LocalTime);
    }
    let text = match values.first() {
        None => clock(realtime).format("%H:%M:%S%.f").to_string(),
        Some(Value::String(value)) => local_time_from_string(value, realtime, clock_function)?,
        Some(Value::LocalTime(value)) => return Ok(Value::LocalTime(value.clone())),
        Some(Value::Time(value)) => LocalTimeValue::from_nanoseconds(value.storage_components().0)?
            .as_str()
            .to_owned(),
        Some(Value::LocalDateTime(value)) => {
            LocalTimeValue::from_nanoseconds(value.storage_components().1)?
                .as_str()
                .to_owned()
        }
        Some(Value::ZonedDateTime(value)) => {
            LocalTimeValue::from_nanoseconds(value.local_components()?.1)?
                .as_str()
                .to_owned()
        }
        Some(Value::Map(value)) => local_time_from_map_input(value, realtime)?,
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => return temporal_type_error("localtime"),
    };
    LocalTimeValue::parse(&text)
        .map(Value::LocalTime)
        .map_err(Into::into)
}

fn local_time_from_string(
    value: &str,
    realtime: bool,
    clock_function: bool,
) -> QueryResult<String> {
    if clock_function {
        return Ok(current_parts(realtime, value)?.1);
    }
    Ok(normalize_local_time_text(value))
}

fn local_time_from_map_input(
    value: &std::collections::BTreeMap<String, Value>,
    realtime: bool,
) -> QueryResult<String> {
    if timezone_only(value) {
        return Ok(current_parts(realtime, timezone(value)?)?.1);
    }
    validate_temporal_map(value, TemporalMapTarget::LocalTime)?;
    local_time_from_map(value)
}

fn time(values: &[Value], realtime: bool, clock_function: bool) -> QueryResult<Value> {
    require_arity(values, 0, if clock_function { 1 } else { 2 })?;
    if values.len() == 2 {
        return parse_temporal_with_pattern(&values[0], &values[1], TemporalTarget::Time);
    }
    let text = match values.first() {
        None => clock(realtime).format("%H:%M:%S%.fZ").to_string(),
        Some(Value::String(zone)) if clock_function => {
            current_time(realtime, zone)?.as_str().to_owned()
        }
        Some(Value::String(value)) => normalize_time_with_default_zone(value),
        Some(Value::Time(value)) => return Ok(Value::Time(value.clone())),
        Some(Value::LocalTime(value)) => {
            return TimeValue::from_components(value.nanoseconds(), 0)
                .map(Value::Time)
                .map_err(Into::into);
        }
        Some(Value::LocalDateTime(value)) => {
            return TimeValue::from_components(value.storage_components().1, 0)
                .map(Value::Time)
                .map_err(Into::into);
        }
        Some(Value::ZonedDateTime(value)) => {
            let (_, nanoseconds) = value.local_components()?;
            return TimeValue::from_components(nanoseconds, value.offset_seconds())
                .map(Value::Time)
                .map_err(Into::into);
        }
        Some(Value::Map(value)) if timezone_only(value) => {
            current_time(realtime, timezone(value)?)?
                .as_str()
                .to_owned()
        }
        Some(Value::Map(value)) => {
            validate_temporal_map(value, TemporalMapTarget::Time)?;
            time_from_map(value, realtime)?
        }
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => return temporal_type_error("time"),
    };
    TimeValue::parse(&text).map(Value::Time).map_err(Into::into)
}

fn zoned_datetime(values: &[Value], realtime: bool, clock_function: bool) -> QueryResult<Value> {
    require_arity(values, 0, if clock_function { 1 } else { 2 })?;
    if values.len() == 2 {
        return parse_temporal_with_pattern(&values[0], &values[1], TemporalTarget::ZonedDateTime);
    }
    if let Some(Value::String(zone)) = values.first()
        && clock_function
    {
        return current_zoned_datetime(realtime, zone).map(Value::ZonedDateTime);
    }
    if let Some(Value::Map(value)) = values.first() {
        return zoned_datetime_from_map(value, realtime).map(Value::ZonedDateTime);
    }
    let text = match values.first() {
        None => clock(realtime).to_rfc3339_opts(SecondsFormat::Nanos, true),
        Some(Value::String(value)) => {
            return zoned_datetime_from_text(value).map(Value::ZonedDateTime);
        }
        Some(Value::ZonedDateTime(value)) => return Ok(Value::ZonedDateTime(value.clone())),
        Some(Value::LocalDateTime(value)) => {
            let (days, nanoseconds) = value.storage_components();
            return ZonedDateTimeValue::from_local(days, nanoseconds, "Z", None)
                .map(Value::ZonedDateTime)
                .map_err(Into::into);
        }
        Some(Value::Date(value)) => {
            return ZonedDateTimeValue::from_local(value.days(), 0, "Z", None)
                .map(Value::ZonedDateTime)
                .map_err(Into::into);
        }
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => return temporal_type_error("datetime"),
    };
    zoned_datetime_from_text(&text).map(Value::ZonedDateTime)
}

fn zoned_datetime_from_text(text: &str) -> QueryResult<ZonedDateTimeValue> {
    let (body, named_zone) = text
        .strip_suffix(']')
        .and_then(|value| value.rsplit_once('['))
        .map_or((text, None), |(value, zone)| (value, Some(zone)));
    let (local, offset) = split_datetime_offset(body)?;
    let local = normalize_local_datetime_text(local)?;
    match (offset, named_zone) {
        (Some(offset), zone) => {
            let offset = normalize_offset_text(offset);
            let zone = zone.unwrap_or(&offset);
            ZonedDateTimeValue::parse(&format!("{local}{offset}"), zone).map_err(Into::into)
        }
        (None, Some(zone)) => {
            let local = LocalDateTimeValue::parse(&local)?;
            let (days, nanoseconds) = local.storage_components();
            ZonedDateTimeValue::from_local(days, nanoseconds, zone, None).map_err(Into::into)
        }
        (None, None) => Err(QueryError::semantic(
            "datetime() input requires a zone or offset",
        )),
    }
}

fn timestamp(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 0, 0)?;
    Ok(Value::Integer(statement_time().timestamp_millis()))
}

fn clock(realtime: bool) -> DateTime<Utc> {
    if realtime {
        Utc::now()
    } else {
        statement_time()
    }
}

fn current_zoned_datetime(realtime: bool, zone: &str) -> QueryResult<ZonedDateTimeValue> {
    let instant = clock(realtime);
    let nanoseconds = i128::from(instant.timestamp()) * 1_000_000_000
        + i128::from(instant.timestamp_subsec_nanos());
    ZonedDateTimeValue::from_instant(nanoseconds, zone).map_err(Into::into)
}

fn current_parts(realtime: bool, zone: &str) -> QueryResult<(String, String, i32)> {
    let current = current_zoned_datetime(realtime, zone)?;
    let (days, nanoseconds) = current.local_components()?;
    Ok((
        DateValue::from_days(days)?.as_str().to_owned(),
        LocalTimeValue::from_nanoseconds(nanoseconds)?
            .as_str()
            .to_owned(),
        current.offset_seconds(),
    ))
}

fn current_time(realtime: bool, zone: &str) -> QueryResult<TimeValue> {
    let (_, local, offset) = current_parts(realtime, zone)?;
    let local = LocalTimeValue::parse(&local)?;
    TimeValue::from_components(local.nanoseconds(), offset).map_err(Into::into)
}

fn timezone_only(map: &std::collections::BTreeMap<String, Value>) -> bool {
    map.len() == 1 && map.contains_key("timezone")
}

fn timezone(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<&str> {
    match map.get("timezone") {
        Some(Value::String(value)) => Ok(value),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "temporal timezone must be String",
        )),
    }
}

fn normalize_date_text(value: &str) -> QueryResult<String> {
    if let Ok(value) = DateValue::parse(value) {
        return Ok(value.as_str().to_owned());
    }
    if let Some(date) = normalized_calendar_date(value)? {
        return Ok(date);
    }
    if let Some(date) = normalized_week_date(value)? {
        return Ok(date);
    }
    if let Some(date) = normalized_quarter_date(value)? {
        return Ok(date);
    }
    if let Some(date) = normalized_ordinal_date(value)? {
        return Ok(date);
    }
    if let Some(date) = normalized_partial_date(value)? {
        return Ok(date);
    }
    DateValue::parse(value)
        .map(|value| value.as_str().to_owned())
        .map_err(Into::into)
}

fn normalized_calendar_date(value: &str) -> QueryResult<Option<String>> {
    let calendar = regex_captures(r"^(\d{4})(\d{2})(\d{2})$", value)?;
    if let Some(values) = calendar {
        return canonical_date(
            parse_i64(&values[1])?,
            parse_u32(&values[2])?,
            parse_u32(&values[3])?,
        )
        .map(Some);
    }
    let month = regex_captures(r"^(\d{4})(\d{2})$", value)?;
    if let Some(values) = month {
        return canonical_date(parse_i64(&values[1])?, parse_u32(&values[2])?, 1).map(Some);
    }
    Ok(None)
}

fn normalized_week_date(value: &str) -> QueryResult<Option<String>> {
    let week = regex_captures(r"^([+-]?\d{4,9})-?W(\d{2})(?:-?([1-7]))?$", value)?;
    if let Some(values) = week {
        let year = parse_i64(&values[1])?;
        let week = parse_u32(&values[2])?;
        let day = match values.get(3) {
            Some(value) => parse_u32(value.as_str())?,
            None => 1,
        };
        return canonical_date_from_days(iso_week_date(year, week, day)?).map(Some);
    }
    Ok(None)
}

fn normalized_quarter_date(value: &str) -> QueryResult<Option<String>> {
    let quarter = regex_captures(r"^([+-]?\d{4,9})-?Q([1-4])(?:-?(\d{2}))?$", value)?;
    if let Some(values) = quarter {
        let year = parse_i64(&values[1])?;
        let quarter = parse_u32(&values[2])?;
        let day = match values.get(3) {
            Some(value) => parse_u32(value.as_str())?,
            None => 1,
        };
        return canonical_date_from_days(quarter_date(year, quarter, day)?).map(Some);
    }
    Ok(None)
}

fn normalized_ordinal_date(value: &str) -> QueryResult<Option<String>> {
    let ordinal = regex_captures(r"^([+-]?\d{4,9})-?(\d{3})$", value)?;
    if let Some(values) = ordinal {
        return canonical_date_from_days(ordinal_date(
            parse_i64(&values[1])?,
            parse_u32(&values[2])?,
        )?)
        .map(Some);
    }
    Ok(None)
}

fn normalized_partial_date(value: &str) -> QueryResult<Option<String>> {
    let partial = regex_captures(r"^([+-]?\d{4,9})-(\d{2})$", value)?;
    if let Some(values) = partial {
        return canonical_date(parse_i64(&values[1])?, parse_u32(&values[2])?, 1).map(Some);
    }
    let year = regex_captures(r"^(\d{4}|[+-]\d{4,9})$", value)?;
    if let Some(values) = year {
        return canonical_date(parse_i64(&values[1])?, 1, 1).map(Some);
    }
    Ok(None)
}

fn local_time_base(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<Option<u64>> {
    map.get("time")
        .or_else(|| map.get("datetime"))
        .map(|value| match value {
            Value::LocalTime(value) => Ok(value.nanoseconds()),
            Value::Time(value) => Ok(value.storage_components().0),
            Value::LocalDateTime(value) => Ok(value.storage_components().1),
            Value::ZonedDateTime(value) => value
                .local_components()
                .map(|parts| parts.1)
                .map_err(Into::into),
            _ => temporal_type_error("time/datetime map base"),
        })
        .transpose()
}

fn split_time_components(nanoseconds: u64) -> (i64, i64, i64, u64) {
    let seconds = nanoseconds / 1_000_000_000;
    (
        (seconds / 3_600) as i64,
        (seconds / 60 % 60) as i64,
        (seconds % 60) as i64,
        nanoseconds % 1_000_000_000,
    )
}

fn map_zone(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<Option<&str>> {
    map.get("timezone").map(|_| timezone(map)).transpose()
}

fn normalize_zoned_extractors(
    map: &std::collections::BTreeMap<String, Value>,
    realtime: bool,
) -> QueryResult<std::collections::BTreeMap<String, Value>> {
    let Some(zone) = map_zone(map)? else {
        return Ok(map.clone());
    };
    let mut normalized = map.clone();
    if let Some(Value::ZonedDateTime(value)) = map.get("datetime") {
        normalized.insert(
            "datetime".to_owned(),
            Value::ZonedDateTime(ZonedDateTimeValue::from_instant(
                value.instant_nanoseconds(),
                zone,
            )?),
        );
    }
    if let Some(value) = map.get("time") {
        let converted = match value {
            Value::Time(value) => {
                let target_offset = current_zoned_datetime(realtime, zone)?.offset_seconds();
                let local = shifted_time_for_offset(
                    value.storage_components().0,
                    value.offset_seconds(),
                    target_offset,
                );
                Some(Value::Time(TimeValue::from_components(
                    local,
                    target_offset,
                )?))
            }
            Value::ZonedDateTime(value) => {
                let converted =
                    ZonedDateTimeValue::from_instant(value.instant_nanoseconds(), zone)?;
                let (_, local) = converted.local_components()?;
                Some(Value::Time(TimeValue::from_components(
                    local,
                    converted.offset_seconds(),
                )?))
            }
            _ => None,
        };
        if let Some(converted) = converted {
            normalized.insert("time".to_owned(), converted);
        }
    }
    Ok(normalized)
}

#[derive(Clone, Copy)]
enum TemporalMapTarget {
    Date,
    LocalTime,
    Time,
    LocalDateTime,
    ZonedDateTime,
}

fn validate_temporal_map(
    map: &std::collections::BTreeMap<String, Value>,
    target: TemporalMapTarget,
) -> QueryResult<()> {
    if map.is_empty() {
        return Err(QueryError::semantic(
            "temporal constructor map must not be empty",
        ));
    }
    validate_temporal_field_names(map, target)?;
    if timezone_only(map) {
        return Ok(());
    }
    validate_timezone_combination(map, target)?;
    validate_temporal_extractors(map)?;
    validate_epoch_fields(map, target)?;
    if !has_epoch_fields(map) {
        if target_has_date(target) {
            validate_date_field_shape(map)?;
        }
        if target_has_time(target) {
            validate_time_field_shape(map)?;
        }
    }
    Ok(())
}

fn validate_temporal_field_names(
    map: &std::collections::BTreeMap<String, Value>,
    target: TemporalMapTarget,
) -> QueryResult<()> {
    let allowed = match target {
        TemporalMapTarget::Date => DATE_FIELDS,
        TemporalMapTarget::LocalTime => LOCAL_TIME_FIELDS,
        TemporalMapTarget::Time => TIME_FIELDS,
        TemporalMapTarget::LocalDateTime => LOCAL_DATETIME_FIELDS,
        TemporalMapTarget::ZonedDateTime => ZONED_DATETIME_FIELDS,
    };
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(QueryError::semantic(format!(
            "temporal field {key} is not available for this constructor"
        )));
    }
    Ok(())
}

fn validate_timezone_combination(
    map: &std::collections::BTreeMap<String, Value>,
    target: TemporalMapTarget,
) -> QueryResult<()> {
    if map.contains_key("timezone")
        && matches!(
            target,
            TemporalMapTarget::Date
                | TemporalMapTarget::LocalTime
                | TemporalMapTarget::LocalDateTime
        )
    {
        return Err(QueryError::semantic(
            "timezone can only be combined with components for a zoned temporal value",
        ));
    }
    Ok(())
}

fn validate_temporal_extractors(
    map: &std::collections::BTreeMap<String, Value>,
) -> QueryResult<()> {
    if map.contains_key("datetime") && (map.contains_key("date") || map.contains_key("time")) {
        return Err(QueryError::semantic(
            "datetime extractor cannot be combined with date or time extractors",
        ));
    }
    Ok(())
}

fn has_epoch_fields(map: &std::collections::BTreeMap<String, Value>) -> bool {
    map.contains_key("epochSeconds") || map.contains_key("epochMillis")
}

fn target_has_date(target: TemporalMapTarget) -> bool {
    matches!(
        target,
        TemporalMapTarget::Date
            | TemporalMapTarget::LocalDateTime
            | TemporalMapTarget::ZonedDateTime
    )
}

fn target_has_time(target: TemporalMapTarget) -> bool {
    matches!(
        target,
        TemporalMapTarget::LocalTime
            | TemporalMapTarget::Time
            | TemporalMapTarget::LocalDateTime
            | TemporalMapTarget::ZonedDateTime
    )
}

const DATE_COMPONENT_FIELDS: &[&str] = &[
    "year",
    "month",
    "day",
    "week",
    "dayOfWeek",
    "quarter",
    "dayOfQuarter",
    "ordinalDay",
];
const TIME_COMPONENT_FIELDS: &[&str] = &[
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
];
const DATE_FIELDS: &[&str] = &[
    "date",
    "year",
    "month",
    "day",
    "week",
    "dayOfWeek",
    "quarter",
    "dayOfQuarter",
    "ordinalDay",
    "timezone",
];
const LOCAL_TIME_FIELDS: &[&str] = &[
    "time",
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
    "timezone",
];
const TIME_FIELDS: &[&str] = &[
    "time",
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
    "timezone",
];
const LOCAL_DATETIME_FIELDS: &[&str] = &[
    "date",
    "time",
    "datetime",
    "year",
    "month",
    "day",
    "week",
    "dayOfWeek",
    "quarter",
    "dayOfQuarter",
    "ordinalDay",
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
    "timezone",
];
const ZONED_DATETIME_FIELDS: &[&str] = &[
    "date",
    "time",
    "datetime",
    "year",
    "month",
    "day",
    "week",
    "dayOfWeek",
    "quarter",
    "dayOfQuarter",
    "ordinalDay",
    "hour",
    "minute",
    "second",
    "millisecond",
    "microsecond",
    "nanosecond",
    "timezone",
    "epochSeconds",
    "epochMillis",
];

fn validate_epoch_fields(
    map: &std::collections::BTreeMap<String, Value>,
    target: TemporalMapTarget,
) -> QueryResult<()> {
    let epoch_seconds = map.contains_key("epochSeconds");
    let epoch_millis = map.contains_key("epochMillis");
    if !epoch_seconds && !epoch_millis {
        return Ok(());
    }
    if !matches!(target, TemporalMapTarget::ZonedDateTime) {
        return Err(QueryError::semantic(
            "epoch fields are only available for datetime()",
        ));
    }
    if epoch_seconds && epoch_millis {
        return Err(QueryError::semantic(
            "epochSeconds and epochMillis cannot be combined",
        ));
    }
    let allowed = ["epochSeconds", "epochMillis", "nanosecond", "timezone"];
    if let Some(key) = map.keys().find(|key| !allowed.contains(&key.as_str())) {
        return Err(QueryError::semantic(format!(
            "temporal field {key} cannot be combined with an epoch field"
        )));
    }
    Ok(())
}

fn validate_date_field_shape(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<()> {
    let has_date_base = map.contains_key("date") || map.contains_key("datetime");
    let has_date_component = DATE_COMPONENT_FIELDS
        .iter()
        .any(|field| map.contains_key(*field));
    if !has_date_base && !has_date_component {
        return Ok(());
    }
    if !has_date_base && !map.contains_key("year") {
        return Err(QueryError::semantic(
            "temporal date components require year",
        ));
    }
    if map.contains_key("day") && !has_date_base && !map.contains_key("month") {
        return Err(QueryError::semantic("temporal field day requires month"));
    }
    if map.contains_key("dayOfWeek") && !map.contains_key("week") {
        return Err(QueryError::semantic(
            "temporal field dayOfWeek requires week",
        ));
    }
    if map.contains_key("dayOfQuarter") && !map.contains_key("quarter") {
        return Err(QueryError::semantic(
            "temporal field dayOfQuarter requires quarter",
        ));
    }
    Ok(())
}

fn validate_time_field_shape(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<()> {
    let has_time_base = map.contains_key("time") || map.contains_key("datetime");
    if has_time_base {
        return Ok(());
    }
    if map.contains_key("minute") && !map.contains_key("hour") {
        return Err(QueryError::semantic("temporal field minute requires hour"));
    }
    if map.contains_key("second") && (!map.contains_key("hour") || !map.contains_key("minute")) {
        return Err(QueryError::semantic(
            "temporal field second requires hour and minute",
        ));
    }
    if TIME_COMPONENT_FIELDS[3..]
        .iter()
        .any(|field| map.contains_key(*field))
        && !map.contains_key("second")
    {
        return Err(QueryError::semantic(
            "temporal fractional fields require second",
        ));
    }
    Ok(())
}

fn local_time_from_map(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<String> {
    LocalTimeValue::from_nanoseconds(time_components(map)?)
        .map(|value| value.as_str().to_owned())
        .map_err(Into::into)
}

fn time_from_map(
    map: &std::collections::BTreeMap<String, Value>,
    realtime: bool,
) -> QueryResult<String> {
    let map = normalize_zoned_extractors(map, realtime)?;
    let offset = match map_zone(&map)? {
        Some(zone) => time_base_offset(&map)
            .unwrap_or(current_zoned_datetime(realtime, zone)?.offset_seconds()),
        None => time_base_offset(&map).unwrap_or(0),
    };
    TimeValue::from_components(time_components(&map)?, offset)
        .map(|value| value.as_str().to_owned())
        .map_err(Into::into)
}

fn time_base_offset(map: &std::collections::BTreeMap<String, Value>) -> Option<i32> {
    match map.get("time").or_else(|| map.get("datetime")) {
        Some(Value::Time(value)) => Some(value.offset_seconds()),
        Some(Value::ZonedDateTime(value)) => Some(value.offset_seconds()),
        _ => None,
    }
}

fn local_datetime_from_map(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<String> {
    let date = date_from_map(map)?;
    let time = LocalTimeValue::from_nanoseconds(time_components(map)?)?;
    Ok(format!("{date}T{}", time.as_str()))
}

fn epoch_nanoseconds(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<Option<i128>> {
    let nanos = optional_integer(map, "nanosecond")?.unwrap_or(0);
    if !(0..1_000_000_000).contains(&nanos) {
        return temporal_type_error("nanosecond must be between 0 and 999999999");
    }
    if let Some(seconds) = optional_integer(map, "epochSeconds")? {
        return Ok(Some(
            i128::from(seconds) * 1_000_000_000 + i128::from(nanos),
        ));
    }
    if let Some(milliseconds) = optional_integer(map, "epochMillis")? {
        return Ok(Some(
            i128::from(milliseconds) * 1_000_000 + i128::from(nanos),
        ));
    }
    Ok(None)
}

fn time_components(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<u64> {
    let base = local_time_base(map)?.unwrap_or(0);
    let (base_hour, base_minute, base_second, base_fraction) = split_time_components(base);
    let hour = optional_integer(map, "hour")?.unwrap_or(base_hour);
    let minute = optional_integer(map, "minute")?.unwrap_or(base_minute);
    let second = optional_integer(map, "second")?.unwrap_or(base_second);
    if !(0..=23).contains(&hour) || !(0..=59).contains(&minute) || !(0..=59).contains(&second) {
        return temporal_type_error("temporal time component is outside its valid range");
    }
    let nanosecond = fractional_nanoseconds(map, base_fraction)?;
    Ok(((hour * 3_600 + minute * 60 + second) as u64) * 1_000_000_000 + nanosecond)
}

fn fractional_nanoseconds(
    map: &std::collections::BTreeMap<String, Value>,
    default: u64,
) -> QueryResult<u64> {
    if !["millisecond", "microsecond", "nanosecond"]
        .into_iter()
        .any(|name| map.contains_key(name))
    {
        return Ok(default);
    }
    let millisecond = optional_integer(map, "millisecond")?.unwrap_or(0);
    let microsecond = optional_integer(map, "microsecond")?.unwrap_or(0);
    let nanosecond = optional_integer(map, "nanosecond")?.unwrap_or(0);
    let supplied = ["millisecond", "microsecond", "nanosecond"]
        .into_iter()
        .filter(|name| map.contains_key(*name))
        .count();
    let maximum = if supplied > 1 { 999 } else { i64::MAX };
    if millisecond < 0
        || microsecond < 0
        || nanosecond < 0
        || millisecond > maximum.min(999)
        || microsecond > maximum.min(999_999)
        || nanosecond > maximum.min(999_999_999)
    {
        return temporal_type_error("temporal fractional component is outside its valid range");
    }
    Ok((millisecond * 1_000_000 + microsecond * 1_000 + nanosecond) as u64)
}

fn optional_integer(
    map: &std::collections::BTreeMap<String, Value>,
    name: &str,
) -> QueryResult<Option<i64>> {
    match map.get(name) {
        None => Ok(None),
        Some(Value::Integer(value)) => Ok(Some(*value)),
        Some(_) => temporal_type_error(&format!("temporal field {name} must be Integer")),
    }
}

#[derive(Clone, Copy)]
enum TemporalTarget {
    Date,
    LocalTime,
    Time,
    LocalDateTime,
    ZonedDateTime,
}

fn parse_temporal_with_pattern(
    input: &Value,
    pattern: &Value,
    target: TemporalTarget,
) -> QueryResult<Value> {
    if matches!(input, Value::Null) || matches!(pattern, Value::Null) {
        return Ok(Value::Null);
    }
    let (Value::String(input), Value::String(pattern)) = (input, pattern) else {
        return temporal_type_error("temporal pattern parsing requires two String values");
    };
    let chrono_pattern = chrono_pattern(pattern)?;
    match target {
        TemporalTarget::Date => {
            let value =
                NaiveDate::parse_from_str(input, &chrono_pattern).map_err(pattern_parse_error)?;
            DateValue::parse(&value.format("%Y-%m-%d").to_string())
                .map(Value::Date)
                .map_err(Into::into)
        }
        TemporalTarget::LocalTime => {
            let value =
                NaiveTime::parse_from_str(input, &chrono_pattern).map_err(pattern_parse_error)?;
            LocalTimeValue::parse(&value.format("%H:%M:%S%.f").to_string())
                .map(Value::LocalTime)
                .map_err(Into::into)
        }
        TemporalTarget::Time => parse_time_pattern(input, &chrono_pattern),
        TemporalTarget::LocalDateTime => {
            let value = parse_local_datetime_pattern(input, &chrono_pattern)?;
            LocalDateTimeValue::parse(&value.format("%Y-%m-%dT%H:%M:%S%.f").to_string())
                .map(Value::LocalDateTime)
                .map_err(Into::into)
        }
        TemporalTarget::ZonedDateTime => parse_zoned_datetime_pattern(input, &chrono_pattern),
    }
}

fn parse_local_datetime_pattern(input: &str, pattern: &str) -> QueryResult<NaiveDateTime> {
    if let Ok(value) = NaiveDateTime::parse_from_str(input, pattern) {
        return Ok(value);
    }
    let date = NaiveDate::parse_from_str(input, pattern).map_err(pattern_parse_error)?;
    date.and_hms_opt(0, 0, 0)
        .ok_or_else(|| QueryError::internal("midnight could not be represented"))
}

fn parse_zoned_datetime_pattern(input: &str, pattern: &str) -> QueryResult<Value> {
    if let Ok(value) = DateTime::<FixedOffset>::parse_from_str(input, pattern) {
        let text = value.format("%Y-%m-%dT%H:%M:%S%.f%:z").to_string();
        let zone = value.format("%:z").to_string();
        return ZonedDateTimeValue::parse(&text, &zone)
            .map(Value::ZonedDateTime)
            .map_err(Into::into);
    }
    let local = parse_local_datetime_pattern(input, pattern)?;
    let days = crate::cypher::days_from_civil(i64::from(local.year()), local.month(), local.day());
    ZonedDateTimeValue::from_local(
        days,
        u64::from(local.time().num_seconds_from_midnight()) * 1_000_000_000
            + u64::from(local.time().nanosecond()),
        "Z",
        None,
    )
    .map(Value::ZonedDateTime)
    .map_err(Into::into)
}

fn parse_time_pattern(input: &str, pattern: &str) -> QueryResult<Value> {
    let input = format!("1970-01-01T{input}");
    let pattern = format!("%Y-%m-%dT{pattern}");
    if let Ok(value) = DateTime::<FixedOffset>::parse_from_str(&input, &pattern) {
        return TimeValue::parse(&value.format("%H:%M:%S%.f%:z").to_string())
            .map(Value::Time)
            .map_err(Into::into);
    }
    let value = NaiveDateTime::parse_from_str(&input, &pattern).map_err(pattern_parse_error)?;
    TimeValue::parse(&format!("{}Z", value.format("%H:%M:%S%.f")))
        .map(Value::Time)
        .map_err(Into::into)
}

fn pattern_parse_error(error: chrono::ParseError) -> QueryError {
    QueryError::new(
        QueryErrorKind::Type,
        format!("temporal input does not match its format pattern: {error}"),
    )
}

fn chrono_pattern(pattern: &str) -> QueryResult<String> {
    let characters = pattern.chars().collect::<Vec<_>>();
    let mut output = String::new();
    let mut index = 0;
    while index < characters.len() {
        let character = characters[index];
        if character == '\'' {
            index = copy_pattern_literal(&characters, index, &mut output)?;
            continue;
        }
        let start = index;
        while index < characters.len() && characters[index] == character {
            index += 1;
        }
        output.push_str(&chrono_pattern_token(character, index - start)?);
    }
    Ok(output)
}

fn copy_pattern_literal(
    characters: &[char],
    start: usize,
    output: &mut String,
) -> QueryResult<usize> {
    let mut index = start + 1;
    while let Some(character) = characters.get(index) {
        if *character == '\'' {
            return Ok(index + 1);
        }
        if *character == '%' {
            output.push('%');
        }
        output.push(*character);
        index += 1;
    }
    Err(QueryError::semantic(
        "unterminated temporal pattern literal",
    ))
}

fn chrono_pattern_token(character: char, count: usize) -> QueryResult<String> {
    let token = match character {
        'G' => match count {
            1..=3 => "AD".to_owned(),
            4 => "Anno Domini".to_owned(),
            5 => "A".to_owned(),
            _ => return Err(QueryError::semantic("unsupported era parsing width")),
        },
        'u' | 'y' => "%Y".to_owned(),
        'Y' if count == 2 => "%g".to_owned(),
        'Y' => "%G".to_owned(),
        'M' | 'L' if count <= 2 => "%m".to_owned(),
        'M' | 'L' if count == 3 => "%b".to_owned(),
        'M' | 'L' if count == 4 => "%B".to_owned(),
        'M' | 'L' => {
            return Err(QueryError::semantic(
                "narrow month names are not unambiguous for temporal parsing",
            ));
        }
        'd' => "%d".to_owned(),
        'D' => "%j".to_owned(),
        'E' if count == 4 => "%A".to_owned(),
        'E' => "%a".to_owned(),
        'e' | 'c' if count <= 2 => "%u".to_owned(),
        'e' | 'c' if count == 4 => "%A".to_owned(),
        'e' | 'c' => "%a".to_owned(),
        'w' => "%V".to_owned(),
        'H' => "%H".to_owned(),
        'k' => "%H".to_owned(),
        'h' | 'K' => "%I".to_owned(),
        'm' => "%M".to_owned(),
        's' => "%S".to_owned(),
        'a' => "%p".to_owned(),
        'S' if (1..=9).contains(&count) => format!("%{count}f"),
        'X' | 'x' | 'Z' if count == 3 => "%:z".to_owned(),
        'X' | 'x' | 'Z' => "%z".to_owned(),
        value if value.is_ascii_alphabetic() => {
            return Err(QueryError::semantic(format!(
                "unsupported temporal parsing pattern component {value}"
            )));
        }
        '%' => "%%".to_owned(),
        value => value.to_string().repeat(count),
    };
    Ok(token)
}

fn date_from_map(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<String> {
    if map.is_empty() {
        return Err(QueryError::semantic(
            "date()/datetime() map must not be empty",
        ));
    }
    let base = map
        .get("date")
        .or_else(|| map.get("datetime"))
        .map(date_value_days)
        .transpose()?;
    let (base_year, base_month, base_day) = base
        .map(crate::cypher::civil_from_days)
        .unwrap_or((0, 1, 1));
    let base_day_of_week = base.map(|days| (days + 3).rem_euclid(7) + 1);
    let base_week_year = base.map(|days| {
        let day_of_week = (days + 3).rem_euclid(7) + 1;
        crate::cypher::civil_from_days(days + (4 - day_of_week)).0
    });
    let base_quarter_start =
        base.map(|_| crate::cypher::days_from_civil(base_year, (base_month - 1) / 3 * 3 + 1, 1));
    let base_day_of_quarter = base
        .zip(base_quarter_start)
        .map(|(days, start)| days - start + 1);
    let week = optional_integer(map, "week")?;
    let quarter = optional_integer(map, "quarter")?;
    let ordinal = optional_integer(map, "ordinalDay")?;
    let inherited_year = if week.is_some() {
        base_week_year
    } else {
        base.map(|_| base_year)
    };
    let year = optional_integer(map, "year")?
        .or(inherited_year)
        .ok_or_else(|| {
            QueryError::new(QueryErrorKind::Type, "temporal field year must be Integer")
        })?;
    DateValue::from_components(year, 1, 1)?;
    if [week, quarter, ordinal].into_iter().flatten().count() > 1 {
        return Err(QueryError::semantic(
            "date map cannot combine week, quarter, and ordinal forms",
        ));
    }
    let days = if let Some(week) = week {
        reject_date_fields(
            map,
            &["month", "day", "quarter", "dayOfQuarter", "ordinalDay"],
        )?;
        let weekday = optional_integer(map, "dayOfWeek")?
            .or(base_day_of_week)
            .unwrap_or(1);
        iso_week_date(
            year,
            unsigned_component(week, "week")?,
            unsigned_component(weekday, "dayOfWeek")?,
        )?
    } else if let Some(quarter) = quarter {
        reject_date_fields(map, &["month", "day", "week", "dayOfWeek", "ordinalDay"])?;
        let day = optional_integer(map, "dayOfQuarter")?
            .or(base_day_of_quarter)
            .unwrap_or(1);
        quarter_date(
            year,
            unsigned_component(quarter, "quarter")?,
            unsigned_component(day, "dayOfQuarter")?,
        )?
    } else if let Some(ordinal) = ordinal {
        reject_date_fields(
            map,
            &[
                "month",
                "day",
                "week",
                "dayOfWeek",
                "quarter",
                "dayOfQuarter",
            ],
        )?;
        ordinal_date(year, unsigned_component(ordinal, "ordinalDay")?)?
    } else {
        if base.is_none() && map.contains_key("day") && !map.contains_key("month") {
            return Err(QueryError::semantic("date map day requires month"));
        }
        let month = optional_integer(map, "month")?.unwrap_or(i64::from(base_month));
        let day = optional_integer(map, "day")?.unwrap_or(i64::from(base_day));
        return canonical_date(
            year,
            unsigned_component(month, "month")?,
            unsigned_component(day, "day")?,
        );
    };
    canonical_date_from_days(days)
}

fn date_value_days(value: &Value) -> QueryResult<i64> {
    match value {
        Value::Date(value) => Ok(value.days()),
        Value::LocalDateTime(value) => Ok(value.storage_components().0),
        Value::ZonedDateTime(value) => value
            .local_components()
            .map(|value| value.0)
            .map_err(Into::into),
        _ => temporal_type_error("date/datetime map base"),
    }
}

fn reject_date_fields(
    map: &std::collections::BTreeMap<String, Value>,
    names: &[&str],
) -> QueryResult<()> {
    if let Some(name) = names.iter().find(|name| map.contains_key(**name)) {
        return Err(QueryError::semantic(format!(
            "temporal field {name} is incompatible with the selected date form"
        )));
    }
    Ok(())
}

fn unsigned_component(value: i64, name: &str) -> QueryResult<u32> {
    u32::try_from(value).map_err(|_| {
        QueryError::new(
            QueryErrorKind::Type,
            format!("temporal field {name} is outside its valid range"),
        )
    })
}

fn iso_week_date(year: i64, week: u32, weekday: u32) -> QueryResult<i64> {
    if week == 0 || week > 53 || !(1..=7).contains(&weekday) {
        return temporal_type_error("week date component");
    }
    let january_fourth = crate::cypher::days_from_civil(year, 1, 4);
    let first_monday = january_fourth - (january_fourth + 3).rem_euclid(7);
    let days = first_monday + i64::from(week - 1) * 7 + i64::from(weekday - 1);
    let thursday = days + (4 - i64::from(weekday));
    if crate::cypher::civil_from_days(thursday).0 != year {
        return temporal_type_error("week date component");
    }
    Ok(days)
}

fn quarter_date(year: i64, quarter: u32, day: u32) -> QueryResult<i64> {
    if !(1..=4).contains(&quarter) || day == 0 {
        return temporal_type_error("quarter date component");
    }
    let month = (quarter - 1) * 3 + 1;
    let start = crate::cypher::days_from_civil(year, month, 1);
    let (next_year, next_month) = if quarter == 4 {
        (year + 1, 1)
    } else {
        (year, month + 3)
    };
    let end = crate::cypher::days_from_civil(next_year, next_month, 1);
    let result = start + i64::from(day - 1);
    if result >= end {
        return temporal_type_error("quarter date component");
    }
    Ok(result)
}

fn ordinal_date(year: i64, ordinal: u32) -> QueryResult<i64> {
    if ordinal == 0 {
        return temporal_type_error("ordinal date component");
    }
    let start = crate::cypher::days_from_civil(year, 1, 1);
    let end = crate::cypher::days_from_civil(year + 1, 1, 1);
    let result = start + i64::from(ordinal - 1);
    if result >= end {
        return temporal_type_error("ordinal date component");
    }
    Ok(result)
}

fn canonical_date(year: i64, month: u32, day: u32) -> QueryResult<String> {
    DateValue::from_components(year, month, day)
        .map(|value| value.as_str().to_owned())
        .map_err(Into::into)
}

fn canonical_date_from_days(days: i64) -> QueryResult<String> {
    DateValue::from_days(days)
        .map(|value| value.as_str().to_owned())
        .map_err(Into::into)
}

fn regex_captures<'a>(pattern: &str, value: &'a str) -> QueryResult<Option<regex::Captures<'a>>> {
    regex::Regex::new(pattern)
        .map_err(|error| QueryError::internal(format!("invalid temporal parser regex: {error}")))
        .map(|regex| regex.captures(value))
}

fn parse_i64(value: &str) -> QueryResult<i64> {
    value
        .parse()
        .map_err(|_| temporal_component_parse_error(value))
}

fn parse_u32(value: &str) -> QueryResult<u32> {
    value
        .parse()
        .map_err(|_| temporal_component_parse_error(value))
}

fn temporal_component_parse_error(value: &str) -> QueryError {
    QueryError::new(
        QueryErrorKind::Type,
        format!("temporal component {value:?} is outside its valid range"),
    )
}

fn temporal_type_error<T>(name: &str) -> QueryResult<T> {
    Err(QueryError::new(
        QueryErrorKind::Type,
        format!("{name}() input has an incompatible type"),
    ))
}

fn local_datetime(values: &[Value], realtime: bool, clock_function: bool) -> QueryResult<Value> {
    require_arity(values, 0, if clock_function { 1 } else { 2 })?;
    if values.len() == 2 {
        return parse_temporal_with_pattern(&values[0], &values[1], TemporalTarget::LocalDateTime);
    }
    let text = match values.first() {
        None => clock(realtime)
            .to_rfc3339_opts(SecondsFormat::Nanos, true)
            .trim_end_matches('Z')
            .to_owned(),
        Some(Value::String(zone)) if clock_function => {
            let (date, time, _) = current_parts(realtime, zone)?;
            date + "T" + &time
        }
        Some(Value::String(value)) => normalize_local_datetime_text(value)?,
        Some(Value::LocalDateTime(value)) => return Ok(Value::LocalDateTime(value.clone())),
        Some(Value::Date(value)) => format!("{}T00:00:00", value.as_str()),
        Some(Value::ZonedDateTime(value)) => {
            let (days, nanoseconds) = value.local_components()?;
            LocalDateTimeValue::from_components(days, nanoseconds)?
                .as_str()
                .to_owned()
        }
        Some(Value::Map(value)) if timezone_only(value) => {
            let (date, time, _) = current_parts(realtime, timezone(value)?)?;
            date + "T" + &time
        }
        Some(Value::Map(value)) => {
            validate_temporal_map(value, TemporalMapTarget::LocalDateTime)?;
            local_datetime_from_map(value)?
        }
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => return temporal_type_error("localdatetime"),
    };
    LocalDateTimeValue::parse(&text)
        .map(Value::LocalDateTime)
        .map_err(Into::into)
}

fn split_datetime_offset(value: &str) -> QueryResult<(&str, Option<&str>)> {
    if let Some(local) = value.strip_suffix('Z') {
        return Ok((local, Some("Z")));
    }
    let time_start = value
        .find('T')
        .ok_or_else(|| QueryError::semantic("datetime() input requires date and time"))?;
    let offset = value[time_start + 1..]
        .char_indices()
        .rev()
        .find(|(_, character)| matches!(character, '+' | '-'))
        .map(|(index, _)| time_start + 1 + index);
    match offset {
        Some(index) => Ok((&value[..index], Some(&value[index..]))),
        None => Ok((value, None)),
    }
}
