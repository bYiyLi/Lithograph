use chrono::{LocalResult, NaiveDate, NaiveTime, Offset, TimeZone, Timelike, Utc};

use super::temporal::{
    DateValue, LocalTimeValue, TimeValue, format_date_from_days, format_local_time, format_offset,
    validate_zone_id,
};
use super::value::ValueError;

const NANOS_PER_DAY: i128 = 86_400 * 1_000_000_000;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LocalDateTimeValue {
    text: String,
    days: i64,
    nanoseconds: u64,
}

impl LocalDateTimeValue {
    pub fn parse(text: &str) -> Result<Self, ValueError> {
        let (date, time) = text.split_once('T').ok_or(ValueError::new(
            "LocalDateTime must contain a date and time separated by T",
        ))?;
        let date = DateValue::parse(date)?;
        let time = LocalTimeValue::parse(time)?;
        Ok(Self {
            text: format!("{}T{}", date.as_str(), time.as_str()),
            days: date.days(),
            nanoseconds: time.nanoseconds(),
        })
    }

    pub fn as_str(&self) -> &str {
        &self.text
    }

    pub(crate) fn comparison_key(&self) -> (i64, u64) {
        (self.days, self.nanoseconds)
    }

    pub(crate) fn storage_components(&self) -> (i64, u64) {
        (self.days, self.nanoseconds)
    }

    pub(crate) fn from_components(days: i64, nanoseconds: u64) -> Result<Self, ValueError> {
        Self::parse(&format!(
            "{}T{}",
            super::temporal::format_date_from_days(days),
            super::temporal::format_local_time(nanoseconds)
        ))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZonedDateTimeValue {
    value: String,
    zone: String,
    instant_nanoseconds: i128,
    offset_seconds: i32,
}

impl ZonedDateTimeValue {
    pub fn parse(value: &str, zone: &str) -> Result<Self, ValueError> {
        let (date_text, time_text) = value.split_once('T').ok_or(ValueError::new(
            "ZonedDateTime must contain a date and time separated by T",
        ))?;
        let date = DateValue::parse(date_text)?;
        let time = TimeValue::parse(time_text)?;
        let instant_nanoseconds = i128::from(date.days()) * NANOS_PER_DAY + time.instant_key();
        let expected_offset = match validate_zone_id(zone)? {
            Some(offset) => offset,
            None => named_zone_offset(zone, instant_nanoseconds)?,
        };
        if expected_offset != time.offset_seconds() {
            return Err(ValueError::new(
                "ZonedDateTime zone rules do not match the value UTC offset",
            ));
        }
        Ok(Self {
            value: format!("{}T{}", date.as_str(), time.as_str()),
            zone: zone.to_owned(),
            instant_nanoseconds,
            offset_seconds: time.offset_seconds(),
        })
    }

    pub fn value(&self) -> &str {
        &self.value
    }

    pub fn zone(&self) -> &str {
        &self.zone
    }

    pub(crate) fn comparison_key(&self) -> (i128, i32, &str) {
        (
            self.instant_nanoseconds,
            self.offset_seconds,
            self.zone.as_str(),
        )
    }

    pub(crate) fn instant_nanoseconds(&self) -> i128 {
        self.instant_nanoseconds
    }

    pub(crate) fn offset_seconds(&self) -> i32 {
        self.offset_seconds
    }

    pub(crate) fn local_components(&self) -> Result<(i64, u64), ValueError> {
        local_components(self.instant_nanoseconds, self.offset_seconds)
    }

    pub(crate) fn from_instant(instant_nanoseconds: i128, zone: &str) -> Result<Self, ValueError> {
        let offset_seconds = match validate_zone_id(zone)? {
            Some(offset) => offset,
            None => named_zone_offset(zone, instant_nanoseconds)?,
        };
        let (days, local_nanoseconds) = local_components(instant_nanoseconds, offset_seconds)?;
        let value = format!(
            "{}T{}{}",
            format_date_from_days(days),
            format_local_time(local_nanoseconds),
            format_offset(offset_seconds)
        );
        Self::parse(&value, zone)
    }

    pub(crate) fn from_local(
        days: i64,
        local_nanoseconds: u64,
        zone: &str,
        preferred_offset: Option<i32>,
    ) -> Result<Self, ValueError> {
        if local_nanoseconds >= NANOS_PER_DAY as u64 {
            return Err(ValueError::new(
                "ZonedDateTime local nanoseconds exceed one day",
            ));
        }
        if let Some(offset) = validate_zone_id(zone)? {
            let instant = i128::from(days) * NANOS_PER_DAY + i128::from(local_nanoseconds)
                - i128::from(offset) * 1_000_000_000;
            return Self::from_instant(instant, zone);
        }
        let timezone = zone
            .parse::<chrono_tz::Tz>()
            .map_err(|_| ValueError::new("ZonedDateTime zone id is not a recognized IANA zone"))?;
        let (year, month, day) = super::temporal::civil_from_days(days);
        let year = i32::try_from(year)
            .map_err(|_| ValueError::new("ZonedDateTime is outside the IANA timezone range"))?;
        let date = NaiveDate::from_ymd_opt(year, month, day)
            .ok_or_else(|| ValueError::new("ZonedDateTime local date is invalid"))?;
        let second = (local_nanoseconds / 1_000_000_000) as u32;
        let nanosecond = (local_nanoseconds % 1_000_000_000) as u32;
        let time = NaiveTime::from_num_seconds_from_midnight_opt(second, nanosecond)
            .ok_or_else(|| ValueError::new("ZonedDateTime local time is invalid"))?;
        let local = date.and_time(time);
        let selected = match timezone.from_local_datetime(&local) {
            LocalResult::Single(value) => value,
            LocalResult::Ambiguous(first, second) => preferred_offset
                .and_then(|preferred| {
                    [first, second]
                        .into_iter()
                        .find(|value| value.offset().fix().local_minus_utc() == preferred)
                })
                .unwrap_or(first),
            LocalResult::None => {
                return Err(ValueError::new(
                    "ZonedDateTime local value falls in a timezone transition gap",
                ));
            }
        };
        let utc = selected.with_timezone(&Utc);
        let instant = i128::from(utc.timestamp()) * 1_000_000_000 + i128::from(utc.nanosecond());
        Self::from_instant(instant, zone)
    }

    pub(crate) fn storage_components(&self) -> Result<(i64, u32, &str), ValueError> {
        let seconds = self.instant_nanoseconds.div_euclid(1_000_000_000);
        let nanoseconds = self.instant_nanoseconds.rem_euclid(1_000_000_000);
        let seconds = i64::try_from(seconds)
            .map_err(|_| ValueError::new("ZonedDateTime exceeds the persistent value range"))?;
        Ok((seconds, nanoseconds as u32, self.zone.as_str()))
    }
}

fn local_components(
    instant_nanoseconds: i128,
    offset_seconds: i32,
) -> Result<(i64, u64), ValueError> {
    let local = instant_nanoseconds
        .checked_add(i128::from(offset_seconds) * 1_000_000_000)
        .ok_or_else(|| ValueError::new("ZonedDateTime local value overflow"))?;
    let days = i64::try_from(local.div_euclid(NANOS_PER_DAY))
        .map_err(|_| ValueError::new("ZonedDateTime exceeds the Cypher date range"))?;
    let nanoseconds = local.rem_euclid(NANOS_PER_DAY) as u64;
    Ok((days, nanoseconds))
}

fn named_zone_offset(zone: &str, instant_nanoseconds: i128) -> Result<i32, ValueError> {
    let timezone = zone
        .parse::<chrono_tz::Tz>()
        .map_err(|_| ValueError::new("ZonedDateTime zone id is not a recognized IANA zone"))?;
    let seconds = i64::try_from(instant_nanoseconds.div_euclid(1_000_000_000))
        .map_err(|_| ValueError::new("ZonedDateTime is outside the IANA timezone range"))?;
    let nanoseconds = instant_nanoseconds.rem_euclid(1_000_000_000) as u32;
    let instant = Utc
        .timestamp_opt(seconds, nanoseconds)
        .single()
        .ok_or_else(|| ValueError::new("ZonedDateTime is outside the IANA timezone range"))?;
    Ok(instant
        .with_timezone(&timezone)
        .offset()
        .fix()
        .local_minus_utc())
}
