use super::temporal::{DateValue, LocalTimeValue, TimeValue, validate_zone_id};
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
        if let Some(fixed_offset) = validate_zone_id(zone)?
            && fixed_offset != time.offset_seconds()
        {
            return Err(ValueError::new(
                "ZonedDateTime fixed zone id does not match the value UTC offset",
            ));
        }
        let instant_nanoseconds = i128::from(date.days()) * NANOS_PER_DAY + time.instant_key();
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
}
