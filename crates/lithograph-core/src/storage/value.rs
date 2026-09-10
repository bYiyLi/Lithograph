use super::encoding::{
    canonical_f32_bits, canonical_f64_bits, decode_counted, decode_i32, decode_i64, decode_u32,
    decode_u64, decode_uleb_field, encode_counted, i32_bytes, i64_bytes, parse_record, record,
    u32_bytes, u64_bytes, uleb_bytes,
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
        let Self::List(values) = self else {
            return true;
        };
        let Some(first) = values.first() else {
            return true;
        };
        let Some(family) = list_element_family(first) else {
            return false;
        };
        values
            .iter()
            .all(|value| list_element_family(value) == Some(family))
    }

    pub(crate) fn type_tag(&self) -> i64 {
        i64::from(self.type_tag_u8())
    }

    pub(crate) fn canonical_bytes(&self) -> StorageResult<Vec<u8>> {
        let fields = match self {
            Self::Boolean(value) => vec![vec![BOOLEAN], vec![u8::from(*value)]],
            Self::Integer(value) => vec![vec![INTEGER], i64_bytes(*value)],
            Self::Float(value) => vec![
                vec![FLOAT],
                canonical_f64_bits(*value).to_le_bytes().to_vec(),
            ],
            Self::String(value) => vec![vec![STRING], value.as_bytes().to_vec()],
            Self::List(values) => {
                let items = values
                    .iter()
                    .map(Self::canonical_bytes)
                    .collect::<StorageResult<Vec<_>>>()?;
                vec![vec![LIST], encode_counted(&items)]
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
                value.zone_id.as_bytes().to_vec(),
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
            Self::Point(value) => point_fields(value),
            Self::Vector(value) => vector_fields(value)?,
            Self::Uuid(value) => vec![vec![UUID], value.to_vec()],
        };
        Ok(record("VALUE", &fields))
    }

    pub(crate) fn from_canonical_bytes(bytes: &[u8]) -> StorageResult<Self> {
        let fields = parse_record(bytes, "VALUE")?;
        let tag_field = fields
            .first()
            .copied()
            .ok_or_else(|| StorageError::corrupt("VALUE record is missing type tag"))?;
        if tag_field.len() != 1 {
            return Err(StorageError::corrupt("VALUE type tag must be one byte"));
        }
        let tag = tag_field[0];
        let expected = expected_field_count(tag)?;
        if fields.len() != expected {
            return Err(StorageError::corrupt(format!(
                "VALUE type tag {tag} requires {expected} field(s), found {}",
                fields.len()
            )));
        }
        let value = decode_value(tag, &fields[1..])?;
        if value.canonical_bytes()? != bytes {
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

fn point_fields(value: &PointValue) -> Vec<Vec<u8>> {
    let mut packed = Vec::with_capacity(value.coordinates.len() * 8);
    for coordinate in &value.coordinates {
        packed.extend_from_slice(&canonical_f64_bits(*coordinate).to_le_bytes());
    }
    vec![
        vec![POINT],
        i64_bytes(value.crs),
        uleb_bytes(value.coordinates.len() as u64),
        packed,
    ]
}

fn vector_fields(value: &VectorValue) -> StorageResult<Vec<Vec<u8>>> {
    Ok(vec![
        vec![VECTOR],
        vec![value.coordinate_type as u8],
        uleb_bytes(value.dimension),
        canonicalize_vector_bytes(value)?,
    ])
}

fn canonicalize_vector_bytes(value: &VectorValue) -> StorageResult<Vec<u8>> {
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
        VectorCoordinateType::F32 => canonicalize_f32_vector(&value.packed),
        VectorCoordinateType::F64 => canonicalize_f64_vector(&value.packed),
        _ => Ok(value.packed.clone()),
    }
}

fn canonicalize_f32_vector(bytes: &[u8]) -> StorageResult<Vec<u8>> {
    let mut output = Vec::with_capacity(bytes.len());
    for chunk in bytes.as_chunks::<4>().0 {
        output.extend_from_slice(&canonical_f32_bits(f32::from_le_bytes(*chunk)).to_le_bytes());
    }
    Ok(output)
}

fn canonicalize_f64_vector(bytes: &[u8]) -> StorageResult<Vec<u8>> {
    let mut output = Vec::with_capacity(bytes.len());
    for chunk in bytes.as_chunks::<8>().0 {
        output.extend_from_slice(&canonical_f64_bits(f64::from_le_bytes(*chunk)).to_le_bytes());
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

fn decode_value(tag: u8, fields: &[&[u8]]) -> StorageResult<PropertyValue> {
    match tag {
        BOOLEAN | INTEGER | FLOAT | STRING | LIST => decode_basic_value(tag, fields),
        DATE | LOCAL_TIME | TIME | LOCAL_DATETIME | ZONED_DATETIME | DURATION => {
            decode_temporal_value(tag, fields)
        }
        POINT | VECTOR | UUID => decode_special_value(tag, fields),
        _ => Err(StorageError::corrupt(format!(
            "unknown property type tag {tag}"
        ))),
    }
}

fn decode_basic_value(tag: u8, fields: &[&[u8]]) -> StorageResult<PropertyValue> {
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
        STRING => Ok(PropertyValue::String(utf8(field(fields, 0)?)?)),
        LIST => decode_list(fields),
        _ => Err(StorageError::corrupt("invalid basic property type tag")),
    }
}

fn decode_temporal_value(tag: u8, fields: &[&[u8]]) -> StorageResult<PropertyValue> {
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
        ZONED_DATETIME => decode_zoned_datetime(fields),
        DURATION => decode_duration(fields),
        _ => Err(StorageError::corrupt("invalid temporal property type tag")),
    }
}

fn decode_special_value(tag: u8, fields: &[&[u8]]) -> StorageResult<PropertyValue> {
    match tag {
        POINT => decode_point(fields),
        VECTOR => decode_vector(fields),
        UUID => decode_uuid(fields),
        _ => Err(StorageError::corrupt("invalid special property type tag")),
    }
}

fn decode_zoned_datetime(fields: &[&[u8]]) -> StorageResult<PropertyValue> {
    Ok(PropertyValue::ZonedDateTime(ZonedDateTimeValue {
        epoch_seconds: decode_i64(field(fields, 0)?)?,
        nanoseconds: decode_u32(field(fields, 1)?)?,
        zone_id: utf8(field(fields, 2)?)?,
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

fn decode_list(fields: &[&[u8]]) -> StorageResult<PropertyValue> {
    let items = decode_counted(field(fields, 0)?)?;
    let values = items
        .into_iter()
        .map(PropertyValue::from_canonical_bytes)
        .collect::<StorageResult<Vec<_>>>()?;
    Ok(PropertyValue::List(values))
}

fn decode_point(fields: &[&[u8]]) -> StorageResult<PropertyValue> {
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
    let mut coordinates = Vec::with_capacity(count);
    for chunk in bytes.as_chunks::<8>().0 {
        coordinates.push(f64::from_bits(u64::from_le_bytes(*chunk)));
    }
    Ok(PropertyValue::Point(PointValue { crs, coordinates }))
}

fn decode_vector(fields: &[&[u8]]) -> StorageResult<PropertyValue> {
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
    let packed = field(fields, 2)?.to_vec();
    let expected = dimension_usize
        .checked_mul(coordinate_width(coordinate_type))
        .ok_or_else(|| StorageError::corrupt("Vector payload length overflows addressable size"))?;
    if packed.len() != expected {
        return Err(StorageError::corrupt(
            "Vector payload length does not match dimension",
        ));
    }
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

fn utf8(bytes: &[u8]) -> StorageResult<String> {
    std::str::from_utf8(bytes)
        .map(str::to_owned)
        .map_err(|_| StorageError::corrupt("VALUE string is not valid UTF-8"))
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
