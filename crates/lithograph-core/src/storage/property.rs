use super::value::{PropertyValue, ZonedDateTimeValue};
use super::{StorageError, StorageResult};

#[derive(Debug, Clone)]
pub(crate) struct PropertyColumns {
    pub(crate) type_tag: i64,
    pub(crate) int_value: Option<i64>,
    pub(crate) real_value: Option<f64>,
    pub(crate) text_value: Option<String>,
    pub(crate) blob_value: Option<Vec<u8>>,
    pub(crate) aux_value: Option<Vec<u8>>,
}

impl PropertyColumns {
    pub(crate) fn from_value(value: &PropertyValue) -> StorageResult<Self> {
        let mut columns = Self::empty(value.type_tag());
        match value {
            PropertyValue::Boolean(value) => columns.int_value = Some(i64::from(*value)),
            PropertyValue::Integer(value) | PropertyValue::Date(value) => {
                columns.int_value = Some(*value);
            }
            PropertyValue::Float(value) => {
                columns.real_value = (!value.is_nan()).then_some(*value);
                columns.aux_value = Some(canonical_float_bits(*value).to_vec());
            }
            PropertyValue::String(value) => columns.text_value = Some(value.clone()),
            PropertyValue::LocalTime(value) => {
                columns.int_value = Some(checked_u64(*value, "LocalTime nanoseconds")?);
            }
            PropertyValue::Time {
                nanoseconds,
                offset_seconds,
            } => {
                columns.int_value = Some(checked_u64(*nanoseconds, "Time nanoseconds")?);
                columns.aux_value = Some(offset_seconds.to_le_bytes().to_vec());
            }
            PropertyValue::LocalDateTime { day, nanoseconds } => {
                columns.int_value = Some(*day);
                columns.aux_value = Some(nanoseconds.to_le_bytes().to_vec());
            }
            PropertyValue::ZonedDateTime(value) => {
                columns.int_value = Some(value.epoch_seconds);
                let mut aux = Vec::with_capacity(4 + value.zone_id.len());
                aux.extend_from_slice(&value.nanoseconds.to_le_bytes());
                aux.extend_from_slice(value.zone_id.as_bytes());
                columns.aux_value = Some(aux);
            }
            PropertyValue::List(_)
            | PropertyValue::Duration { .. }
            | PropertyValue::Point(_)
            | PropertyValue::Vector(_)
            | PropertyValue::Uuid(_) => {
                columns.blob_value = Some(value.canonical_bytes()?);
            }
        }
        Ok(columns)
    }

    pub(crate) fn to_value(&self) -> StorageResult<PropertyValue> {
        self.validate_columns()?;
        match self.type_tag {
            1 => self.boolean_value(),
            2 => Ok(PropertyValue::Integer(self.required_int()?)),
            3 => self.float_value(),
            4 => Ok(PropertyValue::String(self.required_text()?.to_owned())),
            5 => self.complex_value(5),
            6 => Ok(PropertyValue::Date(self.required_int()?)),
            7 => Ok(PropertyValue::LocalTime(self.required_int()? as u64)),
            8 => self.time_value(),
            9 => self.local_datetime_value(),
            10 => self.zoned_datetime_value(),
            11 => self.complex_value(11),
            12 => self.complex_value(12),
            13 => self.complex_value(13),
            14 => self.complex_value(14),
            tag => Err(StorageError::corrupt(format!(
                "property contains unknown type tag {tag}"
            ))),
        }
    }

    fn empty(type_tag: i64) -> Self {
        Self {
            type_tag,
            int_value: None,
            real_value: None,
            text_value: None,
            blob_value: None,
            aux_value: None,
        }
    }

    fn validate_columns(&self) -> StorageResult<()> {
        let valid = match self.type_tag {
            1 | 2 | 6 | 7 => self.only_integer_payload(),
            3 => self.only_float_payload(),
            4 => self.only_text_payload(),
            8..=10 => self.only_temporal_payload(),
            5 | 11..=14 => self.only_blob_payload(),
            _ => false,
        };
        if valid {
            Ok(())
        } else {
            Err(StorageError::corrupt(
                "property payload columns do not match its type tag",
            ))
        }
    }

    fn only_integer_payload(&self) -> bool {
        self.int_value.is_some()
            && self.real_value.is_none()
            && self.text_value.is_none()
            && self.blob_value.is_none()
            && self.aux_value.is_none()
    }

    fn only_float_payload(&self) -> bool {
        self.int_value.is_none()
            && self.text_value.is_none()
            && self.blob_value.is_none()
            && self
                .aux_value
                .as_ref()
                .is_some_and(|value| value.len() == 8)
    }

    fn only_text_payload(&self) -> bool {
        self.int_value.is_none()
            && self.real_value.is_none()
            && self.text_value.is_some()
            && self.blob_value.is_none()
            && self.aux_value.is_none()
    }

    fn only_temporal_payload(&self) -> bool {
        self.int_value.is_some()
            && self.real_value.is_none()
            && self.text_value.is_none()
            && self.blob_value.is_none()
            && self.aux_value.is_some()
    }

    fn only_blob_payload(&self) -> bool {
        self.int_value.is_none()
            && self.real_value.is_none()
            && self.text_value.is_none()
            && self.blob_value.is_some()
            && self.aux_value.is_none()
    }

    fn required_int(&self) -> StorageResult<i64> {
        self.int_value
            .ok_or_else(|| StorageError::corrupt("property is missing integer payload"))
    }

    fn required_text(&self) -> StorageResult<&str> {
        self.text_value
            .as_deref()
            .ok_or_else(|| StorageError::corrupt("property is missing text payload"))
    }

    fn required_aux(&self) -> StorageResult<&[u8]> {
        self.aux_value
            .as_deref()
            .ok_or_else(|| StorageError::corrupt("property is missing auxiliary payload"))
    }

    fn boolean_value(&self) -> StorageResult<PropertyValue> {
        match self.required_int()? {
            0 => Ok(PropertyValue::Boolean(false)),
            1 => Ok(PropertyValue::Boolean(true)),
            value => Err(StorageError::corrupt(format!(
                "Boolean property contains invalid integer payload {value}"
            ))),
        }
    }

    fn float_value(&self) -> StorageResult<PropertyValue> {
        let bits = fixed::<8>(self.required_aux()?, "Float auxiliary payload")?;
        let raw_bits = u64::from_le_bytes(bits);
        let value = f64::from_bits(raw_bits);
        if value.is_nan() {
            if self.real_value.is_some() {
                return Err(StorageError::corrupt(
                    "canonical NaN property must not contain SQLite REAL payload",
                ));
            }
            if raw_bits != 0x7ff8_0000_0000_0000 {
                return Err(StorageError::corrupt(
                    "Float NaN property does not use the canonical binary64 payload",
                ));
            }
        } else if self.real_value != Some(value) {
            return Err(StorageError::corrupt(
                "Float REAL payload does not match canonical numeric value",
            ));
        }
        Ok(PropertyValue::Float(value))
    }

    fn time_value(&self) -> StorageResult<PropertyValue> {
        let offset = i32::from_le_bytes(fixed::<4>(self.required_aux()?, "Time offset payload")?);
        Ok(PropertyValue::Time {
            nanoseconds: checked_i64_to_u64(self.required_int()?, "Time nanoseconds")?,
            offset_seconds: offset,
        })
    }

    fn local_datetime_value(&self) -> StorageResult<PropertyValue> {
        let nanos = u64::from_le_bytes(fixed::<8>(
            self.required_aux()?,
            "LocalDateTime nanoseconds payload",
        )?);
        Ok(PropertyValue::LocalDateTime {
            day: self.required_int()?,
            nanoseconds: nanos,
        })
    }

    fn zoned_datetime_value(&self) -> StorageResult<PropertyValue> {
        let aux = self.required_aux()?;
        if aux.len() < 4 {
            return Err(StorageError::corrupt(
                "ZonedDateTime auxiliary payload is truncated",
            ));
        }
        let nanos = u32::from_le_bytes(fixed::<4>(&aux[..4], "ZonedDateTime nanoseconds")?);
        let zone_id = std::str::from_utf8(&aux[4..])
            .map_err(|_| StorageError::corrupt("ZonedDateTime zone is not valid UTF-8"))?;
        Ok(PropertyValue::ZonedDateTime(ZonedDateTimeValue {
            epoch_seconds: self.required_int()?,
            nanoseconds: nanos,
            zone_id: zone_id.to_owned(),
        }))
    }

    fn complex_value(&self, expected_tag: i64) -> StorageResult<PropertyValue> {
        let blob = self
            .blob_value
            .as_deref()
            .ok_or_else(|| StorageError::corrupt("property is missing canonical blob payload"))?;
        let value = PropertyValue::from_canonical_bytes(blob)?;
        if value.type_tag() != expected_tag {
            return Err(StorageError::corrupt(
                "property canonical blob type does not match type tag",
            ));
        }
        Ok(value)
    }
}

fn canonical_float_bits(value: f64) -> [u8; 8] {
    let bits = if value.is_nan() {
        0x7ff8_0000_0000_0000
    } else {
        value.to_bits()
    };
    bits.to_le_bytes()
}

fn checked_u64(value: u64, name: &str) -> StorageResult<i64> {
    i64::try_from(value).map_err(|_| StorageError::corrupt(format!("{name} exceeds INTEGER64")))
}

fn checked_i64_to_u64(value: i64, name: &str) -> StorageResult<u64> {
    u64::try_from(value).map_err(|_| StorageError::corrupt(format!("{name} must not be negative")))
}

fn fixed<const N: usize>(bytes: &[u8], name: &str) -> StorageResult<[u8; N]> {
    bytes
        .try_into()
        .map_err(|_| StorageError::corrupt(format!("{name} must contain exactly {N} bytes")))
}
