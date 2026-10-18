use super::encoding::{
    borrowed_counted_fields, borrowed_record_fields, canonical_f32_bits, canonical_f64_bits,
    copy_bytes_checked, decode_i32, decode_i64, decode_u32, decode_u64, decode_uleb_field,
    encode_counted_checked, equal_bytes_checked, i32_bytes, i64_bytes, record_checked, u32_bytes,
    u64_bytes, uleb_bytes,
};
use super::{StorageError, StorageResult};

const BOOLEAN: u8 = 1;
const INTEGER: u8 = 2;
const FLOAT: u8 = 3;
const STRING: u8 = 4;
const LIST: u8 = 5;
const DATE: u8 = 6;
const LOCAL_TIME: u8 = 7;
const TIME: u8 = 8;
const LOCAL_DATETIME: u8 = 9;
const ZONED_DATETIME: u8 = 10;
const DURATION: u8 = 11;
const POINT: u8 = 12;
const VECTOR: u8 = 13;
const UUID: u8 = 14;

/// Zoned date-time payload used by persistent properties.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ZonedDateTimeValue {
    pub epoch_seconds: i64,
    pub nanoseconds: u32,
    pub zone_id: String,
}

/// Spatial point payload.
#[derive(Debug, Clone, PartialEq)]
pub struct PointValue {
    pub crs: i64,
    pub coordinates: Vec<f64>,
}

/// Storage-format vector coordinate type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum VectorCoordinateType {
    I8 = 1,
    I16 = 2,
    I32 = 3,
    I64 = 4,
    F32 = 5,
    F64 = 6,
}

/// Typed vector payload retaining the original coordinate width.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VectorValue {
    pub coordinate_type: VectorCoordinateType,
    pub dimension: u64,
    pub packed: Vec<u8>,
}

/// Cypher property value persisted by storage format 1.
#[derive(Debug, Clone, PartialEq)]
pub enum PropertyValue {
    Boolean(bool),
    Integer(i64),
    Float(f64),
    String(String),
    List(Vec<PropertyValue>),
    Date(i64),
    LocalTime(u64),
    Time {
        nanoseconds: u64,
        offset_seconds: i32,
    },
    LocalDateTime {
        day: i64,
        nanoseconds: u64,
    },
    ZonedDateTime(ZonedDateTimeValue),
    Duration {
        months: i64,
        days: i64,
        seconds: i64,
        nanoseconds: i64,
    },
    Point(PointValue),
    Vector(VectorValue),
    Uuid([u8; 16]),
}

impl PropertyValue {
    /// Returns whether this encoded value is legal as a Cypher persistent property value.
    ///
    /// LCE1 can decode a broader tagged List shape because storage format 1 is frozen; callers
    /// at the mutation/type boundary must use this predicate before persisting user data.
    pub fn is_property_value(&self) -> bool {
        self.is_property_value_checked(&|| Ok(())).unwrap_or(false)
    }

    pub(crate) fn is_property_value_checked(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<bool> {
        check()?;
        let Self::List(values) = self else {
            return Ok(true);
        };
        let Some(first) = values.first() else {
            return Ok(true);
        };
        let Some(family) = list_element_family(first) else {
            return Ok(false);
        };
        for value in values {
            check()?;
            if list_element_family(value) != Some(family) {
                return Ok(false);
            }
        }
        Ok(true)
    }

    pub(crate) fn type_tag(&self) -> i64 {
        i64::from(self.type_tag_u8())
    }

    pub(crate) fn canonical_bytes(&self) -> StorageResult<Vec<u8>> {
        self.canonical_bytes_checked(&|| Ok(()))
    }

    pub(crate) fn canonical_bytes_checked(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Vec<u8>> {
        check()?;
        let fields = match self {
            Self::Boolean(value) => vec![vec![BOOLEAN], vec![u8::from(*value)]],
            Self::Integer(value) => vec![vec![INTEGER], i64_bytes(*value)],
            Self::Float(value) => vec![
                vec![FLOAT],
                canonical_f64_bits(*value).to_le_bytes().to_vec(),
            ],
            Self::String(value) => vec![vec![STRING], copy_bytes_checked(value.as_bytes(), check)?],
            Self::List(values) => {
                let mut items = Vec::with_capacity(values.len());
                for value in values {
                    check()?;
                    items.push(value.canonical_bytes_checked(check)?);
                }
                vec![vec![LIST], encode_counted_checked(&items, check)?]
            }
            Self::Date(days) => vec![vec![DATE], i64_bytes(*days)],
            Self::LocalTime(nanos) => vec![vec![LOCAL_TIME], u64_bytes(*nanos)],
            Self::Time {
                nanoseconds,
                offset_seconds,
            } => {
                vec![
                    vec![TIME],
                    u64_bytes(*nanoseconds),
                    i32_bytes(*offset_seconds),
                ]
            }
            Self::LocalDateTime { day, nanoseconds } => {
                vec![
                    vec![LOCAL_DATETIME],
                    i64_bytes(*day),
                    u64_bytes(*nanoseconds),
                ]
            }
            Self::ZonedDateTime(value) => vec![
                vec![ZONED_DATETIME],
                i64_bytes(value.epoch_seconds),
                u32_bytes(value.nanoseconds),
                copy_bytes_checked(value.zone_id.as_bytes(), check)?,
            ],
            Self::Duration {
                months,
                days,
                seconds,
                nanoseconds,
            } => vec![
                vec![DURATION],
                i64_bytes(*months),
                i64_bytes(*days),
                i64_bytes(*seconds),
                i64_bytes(*nanoseconds),
            ],
            Self::Point(value) => point_fields(value, check)?,
            Self::Vector(value) => vector_fields(value, check)?,
            Self::Uuid(value) => vec![vec![UUID], value.to_vec()],
        };
        record_checked("VALUE", &fields, check)
    }

    #[cfg(test)]
    pub(crate) fn from_canonical_bytes(bytes: &[u8]) -> StorageResult<Self> {
        Self::from_canonical_bytes_checked(bytes, &|| Ok(()))
    }

    pub(crate) fn from_canonical_bytes_checked(
        bytes: &[u8],
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<Self> {
        check()?;
        let (fields, count) = borrowed_value_fields(bytes, check)?;
        let tag_field = fields[0];
        if tag_field.len() != 1 {
            return Err(StorageError::corrupt("VALUE type tag must be one byte"));
        }
        let tag = tag_field[0];
        let expected = expected_field_count(tag)?;
        if count != expected {
            return Err(StorageError::corrupt(format!(
                "VALUE type tag {tag} requires {expected} field(s), found {}",
                count
            )));
        }
        let value = decode_value(tag, &fields[1..count], check)?;
        if !equal_bytes_checked(&value.canonical_bytes_checked(check)?, bytes, check)? {
            return Err(StorageError::corrupt(
                "VALUE record is not in canonical LCE1 form",
            ));
        }
        Ok(value)
    }

    fn type_tag_u8(&self) -> u8 {
        match self {
            Self::Boolean(_) => BOOLEAN,
            Self::Integer(_) => INTEGER,
            Self::Float(_) => FLOAT,
            Self::String(_) => STRING,
            Self::List(_) => LIST,
            Self::Date(_) => DATE,
            Self::LocalTime(_) => LOCAL_TIME,
            Self::Time { .. } => TIME,
            Self::LocalDateTime { .. } => LOCAL_DATETIME,
            Self::ZonedDateTime(_) => ZONED_DATETIME,
            Self::Duration { .. } => DURATION,
            Self::Point(_) => POINT,
            Self::Vector(_) => VECTOR,
            Self::Uuid(_) => UUID,
        }
    }
}

fn point_fields(
    value: &PointValue,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<Vec<Vec<u8>>> {
    check()?;
    let mut packed = Vec::with_capacity(value.coordinates.len() * 8);
    for coordinate in &value.coordinates {
        check()?;
        packed.extend_from_slice(&canonical_f64_bits(*coordinate).to_le_bytes());
    }
    Ok(vec![
        vec![POINT],
        i64_bytes(value.crs),
        uleb_bytes(value.coordinates.len() as u64),
        packed,
    ])
}

fn vector_fields(
    value: &VectorValue,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<Vec<Vec<u8>>> {
    Ok(vec![
        vec![VECTOR],
        vec![value.coordinate_type as u8],
        uleb_bytes(value.dimension),
        canonicalize_vector_bytes(value, check)?,
    ])
}

fn canonicalize_vector_bytes(
    value: &VectorValue,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<Vec<u8>> {
    check()?;
    let width = coordinate_width(value.coordinate_type);
    let dimension = usize::try_from(value.dimension)
        .map_err(|_| StorageError::corrupt("Vector dimension exceeds addressable size"))?;
    let expected = dimension
        .checked_mul(width)
        .ok_or_else(|| StorageError::corrupt("Vector dimension overflows addressable size"))?;
    if value.packed.len() != expected {
        return Err(StorageError::corrupt(
            "Vector payload length does not match dimension",
        ));
    }
    match value.coordinate_type {
        VectorCoordinateType::F32 => canonicalize_float_vector(&value.packed, check, |chunk| {
            canonical_f32_bits(f32::from_le_bytes(chunk)).to_le_bytes()
        }),
        VectorCoordinateType::F64 => canonicalize_float_vector(&value.packed, check, |chunk| {
            canonical_f64_bits(f64::from_le_bytes(chunk)).to_le_bytes()
        }),
        _ => copy_bytes_checked(&value.packed, check),
    }
}

fn canonicalize_float_vector<const N: usize>(
    bytes: &[u8],
    check: &dyn Fn() -> StorageResult<()>,
    canonicalize: impl Fn([u8; N]) -> [u8; N],
) -> StorageResult<Vec<u8>> {
    let mut output = copy_bytes_checked(bytes, check)?;
    for chunk in output.as_chunks_mut::<N>().0 {
        check()?;
        *chunk = canonicalize(*chunk);
    }
    Ok(output)
}

fn coordinate_width(coordinate_type: VectorCoordinateType) -> usize {
    match coordinate_type {
        VectorCoordinateType::I8 => 1,
        VectorCoordinateType::I16 => 2,
        VectorCoordinateType::I32 | VectorCoordinateType::F32 => 4,
        VectorCoordinateType::I64 | VectorCoordinateType::F64 => 8,
    }
}

fn list_element_family(value: &PropertyValue) -> Option<u8> {
    match value {
        PropertyValue::Boolean(_) => Some(1),
        PropertyValue::Integer(_) => Some(2),
        PropertyValue::Float(_) => Some(3),
        PropertyValue::String(_) => Some(4),
        PropertyValue::Date(_) => Some(5),
        PropertyValue::LocalTime(_) => Some(6),
        PropertyValue::Time { .. } => Some(7),
        PropertyValue::LocalDateTime { .. } => Some(8),
        PropertyValue::ZonedDateTime(_) => Some(9),
        PropertyValue::Duration { .. } => Some(10),
        PropertyValue::Point(_) => Some(11),
        PropertyValue::Uuid(_) => Some(12),
        PropertyValue::List(_) | PropertyValue::Vector(_) => None,
    }
}

fn decode_value(
    tag: u8,
    fields: &[&[u8]],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    check()?;
    match tag {
        BOOLEAN | INTEGER | FLOAT | STRING | LIST => decode_basic_value(tag, fields, check),
        DATE | LOCAL_TIME | TIME | LOCAL_DATETIME | ZONED_DATETIME | DURATION => {
            decode_temporal_value(tag, fields, check)
        }
        POINT | VECTOR | UUID => decode_special_value(tag, fields, check),
        _ => Err(StorageError::corrupt(format!(
            "unknown property type tag {tag}"
        ))),
    }
}

fn decode_basic_value(
    tag: u8,
    fields: &[&[u8]],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    match tag {
        BOOLEAN => match one_byte(fields, 0)? {
            0 => Ok(PropertyValue::Boolean(false)),
            1 => Ok(PropertyValue::Boolean(true)),
            value => Err(StorageError::corrupt(format!(
                "Boolean VALUE payload must be 0 or 1, found {value}"
            ))),
        },
        INTEGER => Ok(PropertyValue::Integer(decode_i64(field(fields, 0)?)?)),
        FLOAT => Ok(PropertyValue::Float(f64::from_bits(decode_u64(field(
            fields, 0,
        )?)?))),
        STRING => Ok(PropertyValue::String(utf8_checked(
            field(fields, 0)?,
            check,
        )?)),
        LIST => decode_list(fields, check),
        _ => Err(StorageError::corrupt("invalid basic property type tag")),
    }
}

fn decode_temporal_value(
    tag: u8,
    fields: &[&[u8]],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    match tag {
        DATE => Ok(PropertyValue::Date(decode_i64(field(fields, 0)?)?)),
        LOCAL_TIME => Ok(PropertyValue::LocalTime(decode_u64(field(fields, 0)?)?)),
        TIME => Ok(PropertyValue::Time {
            nanoseconds: decode_u64(field(fields, 0)?)?,
            offset_seconds: decode_i32(field(fields, 1)?)?,
        }),
        LOCAL_DATETIME => Ok(PropertyValue::LocalDateTime {
            day: decode_i64(field(fields, 0)?)?,
            nanoseconds: decode_u64(field(fields, 1)?)?,
        }),
        ZONED_DATETIME => decode_zoned_datetime(fields, check),
        DURATION => decode_duration(fields),
        _ => Err(StorageError::corrupt("invalid temporal property type tag")),
    }
}

fn decode_special_value(
    tag: u8,
    fields: &[&[u8]],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    match tag {
        POINT => decode_point(fields, check),
        VECTOR => decode_vector(fields, check),
        UUID => decode_uuid(fields),
        _ => Err(StorageError::corrupt("invalid special property type tag")),
    }
}

fn decode_zoned_datetime(
    fields: &[&[u8]],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    Ok(PropertyValue::ZonedDateTime(ZonedDateTimeValue {
        epoch_seconds: decode_i64(field(fields, 0)?)?,
        nanoseconds: decode_u32(field(fields, 1)?)?,
        zone_id: utf8_checked(field(fields, 2)?, check)?,
    }))
}

fn decode_duration(fields: &[&[u8]]) -> StorageResult<PropertyValue> {
    Ok(PropertyValue::Duration {
        months: decode_i64(field(fields, 0)?)?,
        days: decode_i64(field(fields, 1)?)?,
        seconds: decode_i64(field(fields, 2)?)?,
        nanoseconds: decode_i64(field(fields, 3)?)?,
    })
}

fn decode_list(
    fields: &[&[u8]],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    let mut items = borrowed_counted_fields(field(fields, 0)?)?;
    check()?;
    let mut values = Vec::with_capacity(items.len());
    while let Some(item) = items.next_field()? {
        check()?;
        values.push(PropertyValue::from_canonical_bytes_checked(item, check)?);
    }
    items.finish()?;
    Ok(PropertyValue::List(values))
}

fn decode_point(
    fields: &[&[u8]],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    let crs = decode_i64(field(fields, 0)?)?;
    let count = usize::try_from(decode_uleb_field(field(fields, 1)?)?)
        .map_err(|_| StorageError::corrupt("Point coordinate count exceeds addressable size"))?;
    let bytes = field(fields, 2)?;
    let expected = count
        .checked_mul(8)
        .ok_or_else(|| StorageError::corrupt("Point coordinate payload length overflows"))?;
    if bytes.len() != expected {
        return Err(StorageError::corrupt(
            "Point coordinate payload has invalid length",
        ));
    }
    check()?;
    let mut coordinates = Vec::with_capacity(count);
    for chunk in bytes.as_chunks::<8>().0 {
        check()?;
        coordinates.push(f64::from_bits(u64::from_le_bytes(*chunk)));
    }
    Ok(PropertyValue::Point(PointValue { crs, coordinates }))
}

fn decode_vector(
    fields: &[&[u8]],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<PropertyValue> {
    let coordinate_type = match one_byte(fields, 0)? {
        1 => VectorCoordinateType::I8,
        2 => VectorCoordinateType::I16,
        3 => VectorCoordinateType::I32,
        4 => VectorCoordinateType::I64,
        5 => VectorCoordinateType::F32,
        6 => VectorCoordinateType::F64,
        value => {
            return Err(StorageError::corrupt(format!(
                "unknown vector coordinate tag {value}"
            )));
        }
    };
    let dimension = decode_uleb_field(field(fields, 1)?)?;
    let dimension_usize = usize::try_from(dimension)
        .map_err(|_| StorageError::corrupt("Vector dimension exceeds addressable size"))?;
    let bytes = field(fields, 2)?;
    let expected = dimension_usize
        .checked_mul(coordinate_width(coordinate_type))
        .ok_or_else(|| StorageError::corrupt("Vector payload length overflows addressable size"))?;
    if bytes.len() != expected {
        return Err(StorageError::corrupt(
            "Vector payload length does not match dimension",
        ));
    }
    let packed = copy_bytes_checked(bytes, check)?;
    Ok(PropertyValue::Vector(VectorValue {
        coordinate_type,
        dimension,
        packed,
    }))
}

fn decode_uuid(fields: &[&[u8]]) -> StorageResult<PropertyValue> {
    let bytes: [u8; 16] = field(fields, 0)?
        .try_into()
        .map_err(|_| StorageError::corrupt("UUID property must contain exactly 16 bytes"))?;
    Ok(PropertyValue::Uuid(bytes))
}

fn field<'a>(fields: &'a [&'a [u8]], index: usize) -> StorageResult<&'a [u8]> {
    fields
        .get(index)
        .copied()
        .ok_or_else(|| StorageError::corrupt("VALUE record is missing a required field"))
}

fn one_byte(fields: &[&[u8]], index: usize) -> StorageResult<u8> {
    let bytes = field(fields, index)?;
    if bytes.len() != 1 {
        return Err(StorageError::corrupt(
            "VALUE field must contain exactly one byte",
        ));
    }
    Ok(bytes[0])
}

pub(super) fn borrowed_value_fields<'a>(
    bytes: &'a [u8],
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<([&'a [u8]; 5], usize)> {
    let mut input = borrowed_record_fields(bytes, "VALUE")?;
    let count = input.len();
    if count == 0 || count > 5 {
        return Err(StorageError::corrupt(
            "VALUE record has invalid field count",
        ));
    }
    let mut fields = [&[][..]; 5];
    for field in &mut fields[..count] {
        check()?;
        *field = input
            .next_field()?
            .ok_or_else(|| StorageError::corrupt("VALUE record is missing a field"))?;
    }
    input.finish()?;
    Ok((fields, count))
}

fn utf8_checked(bytes: &[u8], check: &dyn Fn() -> StorageResult<()>) -> StorageResult<String> {
    check()?;
    let mut output = String::with_capacity(bytes.len());
    let mut start = 0;
    while start < bytes.len() {
        check()?;
        let end = start.saturating_add(64 * 1024).min(bytes.len());
        let chunk = &bytes[start..end];
        let valid = match std::str::from_utf8(chunk) {
            Ok(value) => value,
            Err(error) if error.error_len().is_none() && end < bytes.len() => {
                std::str::from_utf8(&chunk[..error.valid_up_to()])
                    .map_err(|_| StorageError::corrupt("VALUE string is not valid UTF-8"))?
            }
            Err(_) => return Err(StorageError::corrupt("VALUE string is not valid UTF-8")),
        };
        output.push_str(valid);
        start += valid.len();
    }
    check()?;
    Ok(output)
}

fn expected_field_count(tag: u8) -> StorageResult<usize> {
    match tag {
        BOOLEAN | INTEGER | FLOAT | STRING | LIST | DATE | LOCAL_TIME | UUID => Ok(2),
        TIME | LOCAL_DATETIME => Ok(3),
        ZONED_DATETIME | POINT | VECTOR => Ok(4),
        DURATION => Ok(5),
        _ => Err(StorageError::corrupt(format!(
            "unknown property type tag {tag}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::super::encoding::record;
    use super::*;

    #[test]
    fn checked_list_decode_and_encode_observe_cancellation_during_actual_work() {
        let value = PropertyValue::List(vec![PropertyValue::Integer(1); 10_000]);
        let bytes = value.canonical_bytes().unwrap();
        let calls = Cell::new(0);
        let check = || {
            calls.set(calls.get() + 1);
            if calls.get() == 257 {
                Err(StorageError::not_found("cancel actual canonical work"))
            } else {
                Ok(())
            }
        };
        assert!(PropertyValue::from_canonical_bytes_checked(&bytes, &check).is_err());
        assert_eq!(calls.get(), 257);
        calls.set(0);
        assert!(value.canonical_bytes_checked(&check).is_err());
        assert_eq!(calls.get(), 257);
    }

    #[test]
    fn checked_utf8_decode_preserves_multibyte_boundaries_and_rejects_invalid_bytes() {
        let text = "人物🦀".repeat(20_000);
        let value = PropertyValue::String(text.clone());
        let bytes = value.canonical_bytes().unwrap();
        assert_eq!(PropertyValue::from_canonical_bytes(&bytes).unwrap(), value);
        assert_eq!(utf8_checked(text.as_bytes(), &|| Ok(())).unwrap(), text);
        assert!(utf8_checked(&[0xf0, 0x90, 0x80], &|| Ok(())).is_err());
        assert!(utf8_checked(&[0xff], &|| Ok(())).is_err());
    }

    #[test]
    fn checked_encoding_matches_frozen_lce1_integer_and_list_bytes() {
        let integer = record("VALUE", &[vec![INTEGER], 3_i64.to_le_bytes().to_vec()]);
        assert_eq!(
            PropertyValue::Integer(3).canonical_bytes().unwrap(),
            integer
        );
        let encoded = record(
            "VALUE",
            &[
                vec![LIST],
                super::super::encoding::encode_counted(&[integer]),
            ],
        );
        let value = PropertyValue::List(vec![PropertyValue::Integer(3)]);
        assert_eq!(value.canonical_bytes_checked(&|| Ok(())).unwrap(), encoded);
        assert_eq!(
            PropertyValue::from_canonical_bytes_checked(&encoded, &|| Ok(())).unwrap(),
            value
        );
    }
}
