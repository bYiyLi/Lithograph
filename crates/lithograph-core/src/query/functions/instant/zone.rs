use super::*;

pub(super) fn normalize_time_with_default_zone(value: &str) -> String {
    let normalized = normalize_zoned_time_text(value);
    if normalized.ends_with('Z')
        || normalized
            .char_indices()
            .skip(1)
            .any(|(_, character)| matches!(character, '+' | '-'))
    {
        normalized
    } else {
        format!("{}Z", normalize_local_time_text(value))
    }
}

pub(super) fn temporal_base_zone(
    map: &std::collections::BTreeMap<String, Value>,
) -> Option<String> {
    match map.get("datetime").or_else(|| map.get("time")) {
        Some(Value::ZonedDateTime(value)) => Some(value.zone().to_owned()),
        Some(Value::Time(value)) => Some(crate::cypher::format_offset(value.offset_seconds())),
        _ => None,
    }
}

pub(super) fn normalize_zoned_datetime_time_for_date(
    map: &std::collections::BTreeMap<String, Value>,
    date_days: i64,
) -> QueryResult<std::collections::BTreeMap<String, Value>> {
    let Some(zone) = map_zone(map)? else {
        return Ok(map.clone());
    };
    let Some(value) = map.get("time") else {
        return Ok(map.clone());
    };
    let (local_nanoseconds, source_zone, preferred_offset) = match value {
        Value::Time(value) => (
            value.storage_components().0,
            crate::cypher::format_offset(value.offset_seconds()),
            Some(value.offset_seconds()),
        ),
        Value::ZonedDateTime(value) => (
            value.local_components()?.1,
            value.zone().to_owned(),
            Some(value.offset_seconds()),
        ),
        _ => return Ok(map.clone()),
    };
    let source = ZonedDateTimeValue::from_local(
        date_days,
        local_nanoseconds,
        &source_zone,
        preferred_offset,
    )?;
    let converted = ZonedDateTimeValue::from_instant(source.instant_nanoseconds(), zone)?;
    let (_, local_nanoseconds) = converted.local_components()?;
    let mut normalized = map.clone();
    normalized.insert(
        "time".to_owned(),
        Value::Time(TimeValue::from_components(
            local_nanoseconds,
            converted.offset_seconds(),
        )?),
    );
    Ok(normalized)
}

pub(super) fn shifted_time_for_offset(local: u64, source_offset: i32, target_offset: i32) -> u64 {
    const NANOS_PER_DAY: i128 = 86_400 * 1_000_000_000;
    (i128::from(local) + i128::from(target_offset - source_offset) * 1_000_000_000)
        .rem_euclid(NANOS_PER_DAY) as u64
}

pub(super) fn zoned_datetime_from_map(
    map: &std::collections::BTreeMap<String, Value>,
    realtime: bool,
) -> QueryResult<ZonedDateTimeValue> {
    validate_temporal_map(map, TemporalMapTarget::ZonedDateTime)?;
    if timezone_only(map) {
        return current_zoned_datetime(realtime, timezone(map)?);
    }
    let zone = zoned_datetime_zone(map)?;
    if let Some(epoch) = epoch_nanoseconds(map)? {
        return ZonedDateTimeValue::from_instant(epoch, &zone).map_err(Into::into);
    }
    let normalized = normalize_zoned_datetime_map(map, realtime)?;
    let date = DateValue::parse(&date_from_map(&normalized)?)?;
    ZonedDateTimeValue::from_local(date.days(), time_components(&normalized)?, &zone, None)
        .map_err(Into::into)
}

fn zoned_datetime_zone(map: &std::collections::BTreeMap<String, Value>) -> QueryResult<String> {
    Ok(map_zone(map)?
        .map(str::to_owned)
        .or_else(|| temporal_base_zone(map))
        .unwrap_or_else(|| "Z".to_owned()))
}

fn normalize_zoned_datetime_map(
    map: &std::collections::BTreeMap<String, Value>,
    realtime: bool,
) -> QueryResult<std::collections::BTreeMap<String, Value>> {
    if map.contains_key("time") && map.contains_key("timezone") {
        let date = DateValue::parse(&date_from_map(map)?)?;
        normalize_zoned_datetime_time_for_date(map, date.days())
    } else {
        normalize_zoned_extractors(map, realtime)
    }
}
