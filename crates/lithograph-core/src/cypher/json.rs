use std::collections::BTreeMap;

use serde_json::{Map, Number, Value as JsonValue, json};

use super::temporal::{
    DateValue, DurationValue, LocalDateTimeValue, LocalTimeValue, TimeValue, ZonedDateTimeValue,
};
use super::value::{
    NodeValue, PathValue, PointValue, RelationshipValue, UuidValue, Value, ValueError,
    VectorCoordinateType, VectorValue, VectorValues,
};

const JS_SAFE_INTEGER: i64 = 9_007_199_254_740_991;

type DecodeCheckpoint<'a> = dyn FnMut(usize) -> Result<(), ValueError> + 'a;

pub fn decode_json(value: &JsonValue) -> Result<Value, ValueError> {
    decode_json_internal(value, &mut |_| Ok(()))
}

/// Decode standard Lithograph JSON with a caller-owned cancellation callback.
/// Callback errors retain their original type; invalid values convert through
/// `From<ValueError>`. Zero requests an immediate checkpoint.
pub fn decode_json_checked<E: From<ValueError>>(
    value: &JsonValue,
    checkpoint: &mut impl FnMut(usize) -> Result<(), E>,
) -> Result<Value, E> {
    with_decode_checkpoint(checkpoint, |checkpoint| {
        decode_json_internal(value, checkpoint)
    })
}

fn with_decode_checkpoint<T, E: From<ValueError>>(
    checkpoint: &mut impl FnMut(usize) -> Result<(), E>,
    decode: impl FnOnce(&mut DecodeCheckpoint<'_>) -> Result<T, ValueError>,
) -> Result<T, E> {
    let mut physical_error = None;
    let result = decode(&mut |units| match checkpoint(units) {
        Ok(()) => Ok(()),
        Err(error) => {
            physical_error = Some(error);
            Err(ValueError::new("value decoding checkpoint failed"))
        }
    });
    match physical_error {
        Some(error) => Err(error),
        None => result.map_err(E::from),
    }
}

fn decode_json_internal(
    value: &JsonValue,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    checkpoint(1)?;
    match value {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(value) => Ok(Value::Boolean(*value)),
        JsonValue::Number(value) => decode_number(value),
        JsonValue::String(value) => decode_owned_text(value, checkpoint).map(Value::String),
        JsonValue::Array(values) => values
            .iter()
            .map(|value| decode_json_internal(value, checkpoint))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        JsonValue::Object(value) => decode_object(value, checkpoint),
    }
}

fn decode_owned_text(
    value: &str,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<String, ValueError> {
    checked_text(value, &mut |work, bytes| {
        checkpoint(if bytes == 0 {
            work as usize
        } else {
            bytes as usize
        })
    })
}

pub fn encode_json(value: &Value) -> JsonValue {
    match encode_json_checked(value, &mut |_, _| Ok::<(), std::convert::Infallible>(())) {
        Ok(value) => value,
        Err(never) => match never {},
    }
}

/// Encode using the standard tagged JSON shapes while checking a caller-owned
/// execution callback at every value and each 64 KiB of owned string copying.
/// The callback receives (work units, copied bytes); (0, 0) requests an immediate
/// check around a large allocation/copy, without coupling Cypher
/// value encoding to a particular query executor or resource implementation.
pub fn encode_json_checked<E>(
    value: &Value,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
) -> Result<JsonValue, E> {
    encode_json_filtered_checked(value, checkpoint, &|_| true)
}

/// The property predicate is applied only to graph-element properties; plain
/// Cypher maps keep their keys. Filtering happens during encoding and avoids
/// creating a second owned copy of the graph value.
pub fn encode_json_filtered_checked<E>(
    value: &Value,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
    allow_property: &dyn Fn(&str) -> bool,
) -> Result<JsonValue, E> {
    checkpoint(1, 0)?;
    match value {
        Value::Null => Ok(JsonValue::Null),
        Value::Boolean(value) => Ok(JsonValue::Bool(*value)),
        Value::Integer(value) if (-JS_SAFE_INTEGER..=JS_SAFE_INTEGER).contains(value) => {
            Ok(JsonValue::Number((*value).into()))
        }
        Value::Integer(value) => tagged_text_checked("Integer", &value.to_string(), checkpoint),
        Value::Float(value) if value.is_finite() => Ok(Number::from_f64(*value)
            .map(JsonValue::Number)
            .unwrap_or(JsonValue::Null)),
        Value::Float(value) => encode_nonfinite_float(*value, checkpoint),
        Value::String(value) => checked_string(value, checkpoint),
        Value::List(values) => values
            .iter()
            .map(|value| encode_json_filtered_checked(value, checkpoint, allow_property))
            .collect::<Result<_, _>>()
            .map(JsonValue::Array),
        Value::Map(values) => encode_map_checked(values, checkpoint, allow_property),
        Value::Node(value) => encode_node_checked(value, checkpoint, allow_property),
        Value::Relationship(value) => {
            encode_relationship_checked(value, checkpoint, allow_property)
        }
        Value::Path(value) => encode_path_checked(value, checkpoint, allow_property),
        Value::Date(value) => tagged_text_checked("Date", value.as_str(), checkpoint),
        Value::LocalTime(value) => tagged_text_checked("LocalTime", value.as_str(), checkpoint),
        Value::Time(value) => tagged_text_checked("Time", value.as_str(), checkpoint),
        Value::LocalDateTime(value) => {
            tagged_text_checked("LocalDateTime", value.as_str(), checkpoint)
        }
        Value::ZonedDateTime(value) => Ok(object_json([
            ("$type", JsonValue::String("ZonedDateTime".to_owned())),
            ("value", checked_string(value.value(), checkpoint)?),
            ("zone", checked_string(value.zone(), checkpoint)?),
        ])),
        Value::Duration(value) => tagged_text_checked("Duration", value.as_str(), checkpoint),
        Value::Point(value) => {
            Ok(json!({"$type":"Point", "crs":value.crs(), "coordinates":value.coordinates()}))
        }
        Value::Vector(value) => encode_vector_checked(value, checkpoint, allow_property),
        Value::Uuid(value) => tagged_text_checked("UUID", &value.to_canonical(), checkpoint),
    }
}

fn encode_nonfinite_float<E>(
    value: f64,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
) -> Result<JsonValue, E> {
    let text = if value.is_nan() {
        "NaN"
    } else if value.is_sign_positive() {
        "Infinity"
    } else {
        "-Infinity"
    };
    tagged_text_checked("Float", text, checkpoint)
}

fn encode_map_checked<E>(
    values: &BTreeMap<String, Value>,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
    allow_property: &dyn Fn(&str) -> bool,
) -> Result<JsonValue, E> {
    let entries = map_json_checked(values, checkpoint, allow_property, false)?;
    Ok(if values.contains_key("$type") {
        object_json([
            ("$type", JsonValue::String("Map".to_owned())),
            ("entries", entries),
        ])
    } else {
        entries
    })
}

fn encode_path_checked<E>(
    value: &PathValue,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
    allow_property: &dyn Fn(&str) -> bool,
) -> Result<JsonValue, E> {
    Ok(object_json([
        ("$type", JsonValue::String("Path".to_owned())),
        (
            "nodes",
            JsonValue::Array(
                value
                    .nodes
                    .iter()
                    .map(|node| encode_node_checked(node, checkpoint, allow_property))
                    .collect::<Result<_, _>>()?,
            ),
        ),
        (
            "relationships",
            JsonValue::Array(
                value
                    .relationships
                    .iter()
                    .map(|relationship| {
                        encode_relationship_checked(relationship, checkpoint, allow_property)
                    })
                    .collect::<Result<_, _>>()?,
            ),
        ),
    ]))
}

pub fn decode_parameters(value: &JsonValue) -> Result<BTreeMap<String, Value>, ValueError> {
    decode_parameters_internal(value, &mut |_| Ok(()))
}

/// Decode parameters using the same value decoder and typed conversion paths as
/// the legacy API while preserving a caller's physical cancellation error.
pub fn decode_parameters_checked<E: From<ValueError>>(
    value: &JsonValue,
    checkpoint: &mut impl FnMut(usize) -> Result<(), E>,
) -> Result<BTreeMap<String, Value>, E> {
    with_decode_checkpoint(checkpoint, |checkpoint| {
        decode_parameters_internal(value, checkpoint)
    })
}

fn decode_parameters_internal(
    value: &JsonValue,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<BTreeMap<String, Value>, ValueError> {
    checkpoint(0)?;
    let object = value
        .as_object()
        .ok_or_else(|| ValueError::new("Cypher parameters must be a JSON object"))?;
    object.iter().map(|(name, raw)| {
        let value = decode_json_internal(raw, checkpoint)?;
        if !parameter_value_checked(&value, checkpoint)? {
            return Err(ValueError::new(format!("parameter {name:?} uses a structural value that cannot be supplied as a Cypher parameter")));
        }
        Ok((decode_owned_text(name, checkpoint)?, value))
    }).collect()
}

fn parameter_value_checked(
    value: &Value,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<bool, ValueError> {
    checkpoint(1)?;
    match value {
        Value::Node(_) | Value::Relationship(_) | Value::Path(_) => Ok(false),
        Value::List(values) => {
            for value in values {
                if !parameter_value_checked(value, checkpoint)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        Value::Map(values) => {
            for value in values.values() {
                if !parameter_value_checked(value, checkpoint)? {
                    return Ok(false);
                }
            }
            Ok(true)
        }
        _ => Ok(true),
    }
}

pub fn decode_parameters_text(text: &str) -> Result<BTreeMap<String, Value>, ValueError> {
    let value: JsonValue = serde_json::from_str(text)
        .map_err(|error| ValueError::new(format!("invalid parameters JSON: {error}")))?;
    decode_parameters(&value)
}

pub fn decode_json_text(text: &str) -> Result<Value, ValueError> {
    let value: JsonValue = serde_json::from_str(text)
        .map_err(|error| ValueError::new(format!("invalid JSON value: {error}")))?;
    decode_json(&value)
}

pub fn encode_json_text(value: &Value) -> String {
    encode_json(value).to_string()
}

fn decode_number(value: &Number) -> Result<Value, ValueError> {
    if let Some(value) = value.as_i64() {
        Ok(Value::Integer(value))
    } else if let Some(value) = value.as_f64() {
        Ok(Value::Float(value))
    } else {
        Err(ValueError::new(
            "JSON number is outside the supported Cypher numeric range",
        ))
    }
}

fn decode_object(
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    let Some(tag) = value.get("$type") else {
        return decode_plain_map(value, checkpoint);
    };
    let tag = tag
        .as_str()
        .ok_or_else(|| ValueError::new("$type must be a string"))?;
    if let Some(decoded) = decode_numeric_or_map_tag(tag, value, checkpoint)? {
        return Ok(decoded);
    }
    if let Some(decoded) = decode_structural_tag(tag, value, checkpoint)? {
        return Ok(decoded);
    }
    if let Some(decoded) = decode_basic_temporal_tag(tag, value)? {
        return Ok(decoded);
    }
    if let Some(decoded) = decode_datetime_duration_tag(tag, value, checkpoint)? {
        return Ok(decoded);
    }
    if let Some(decoded) = decode_spatial_vector_uuid_tag(tag, value, checkpoint)? {
        return Ok(decoded);
    }
    Err(ValueError::new(format!(
        "unknown Lithograph JSON $type {tag:?}"
    )))
}

fn decode_numeric_or_map_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Option<Value>, ValueError> {
    match tag {
        "Integer" => {
            require_exact_fields(value, &["$type", "value"])?;
            decode_integer(value).map(Some)
        }
        "Float" => {
            require_exact_fields(value, &["$type", "value"])?;
            decode_float(value).map(Some)
        }
        "Map" => {
            require_exact_fields(value, &["$type", "entries"])?;
            decode_wrapped_map(value, checkpoint).map(Some)
        }
        _ => Ok(None),
    }
}

fn decode_structural_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Option<Value>, ValueError> {
    match tag {
        "Node" => {
            require_exact_fields(value, &["$type", "elementId", "labels", "properties"])?;
            decode_node(value, checkpoint).map(Some)
        }
        "Relationship" => {
            require_exact_fields(
                value,
                &["$type", "elementId", "type", "start", "end", "properties"],
            )?;
            decode_relationship(value, checkpoint).map(Some)
        }
        "Path" => {
            require_exact_fields(value, &["$type", "nodes", "relationships"])?;
            decode_path(value, checkpoint).map(Some)
        }
        _ => Ok(None),
    }
}

fn decode_basic_temporal_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
) -> Result<Option<Value>, ValueError> {
    match tag {
        "Date" => {
            require_exact_fields(value, &["$type", "value"])?;
            Ok(Some(Value::Date(DateValue::parse(&required_string(
                value, "value",
            )?)?)))
        }
        "LocalTime" => {
            require_exact_fields(value, &["$type", "value"])?;
            Ok(Some(Value::LocalTime(LocalTimeValue::parse(
                &required_string(value, "value")?,
            )?)))
        }
        "Time" => {
            require_exact_fields(value, &["$type", "value"])?;
            Ok(Some(Value::Time(TimeValue::parse(required_text_ref(
                value, "value",
            )?)?)))
        }
        _ => Ok(None),
    }
}

fn decode_datetime_duration_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Option<Value>, ValueError> {
    match tag {
        "LocalDateTime" => {
            require_exact_fields(value, &["$type", "value"])?;
            Ok(Some(Value::LocalDateTime(LocalDateTimeValue::parse(
                &required_string(value, "value")?,
            )?)))
        }
        "ZonedDateTime" => {
            require_exact_fields(value, &["$type", "value", "zone"])?;
            Ok(Some(Value::ZonedDateTime(ZonedDateTimeValue::parse(
                &required_string(value, "value")?,
                &required_string(value, "zone")?,
            )?)))
        }
        "Duration" => {
            require_exact_fields(value, &["$type", "value"])?;
            Ok(Some(Value::Duration(DurationValue::parse_with_checkpoint(
                required_text_ref(value, "value")?,
                checkpoint,
            )?)))
        }
        _ => Ok(None),
    }
}

fn decode_spatial_vector_uuid_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Option<Value>, ValueError> {
    match tag {
        "Point" => {
            require_exact_fields(value, &["$type", "crs", "coordinates"])?;
            decode_point(value, checkpoint).map(Some)
        }
        "Vector" => {
            require_exact_fields(value, &["$type", "coordinateType", "dimension", "values"])?;
            decode_vector(value, checkpoint).map(Some)
        }
        "UUID" => {
            require_exact_fields(value, &["$type", "value"])?;
            Ok(Some(Value::Uuid(UuidValue::parse(&required_string(
                value, "value",
            )?)?)))
        }
        _ => Ok(None),
    }
}

fn decode_plain_map(
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    value
        .iter()
        .map(|(key, value)| {
            Ok((
                decode_owned_text(key, checkpoint)?,
                decode_json_internal(value, checkpoint)?,
            ))
        })
        .collect::<Result<BTreeMap<_, _>, ValueError>>()
        .map(Value::Map)
}

fn decode_wrapped_map(
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    let entries = value
        .get("entries")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| ValueError::new("Map wrapper requires an object-valued entries field"))?;
    decode_plain_map(entries, checkpoint)
}

fn decode_integer(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    let text = required_string(value, "value")?;
    text.parse::<i64>()
        .map(Value::Integer)
        .map_err(|_| ValueError::new("tagged Integer is outside INTEGER64 range"))
}

fn decode_float(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    match required_string(value, "value")?.as_str() {
        "NaN" => Ok(Value::Float(f64::NAN)),
        "Infinity" => Ok(Value::Float(f64::INFINITY)),
        "-Infinity" => Ok(Value::Float(f64::NEG_INFINITY)),
        _ => Err(ValueError::new(
            "tagged Float must be NaN, Infinity, or -Infinity",
        )),
    }
}

fn decode_node(
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    let labels = required_array(value, "labels")?
        .iter()
        .map(|value| {
            checkpoint(1)?;
            let text = value
                .as_str()
                .ok_or_else(|| ValueError::new("Node labels must be strings"))?;
            decode_owned_text(text, checkpoint)
        })
        .collect::<Result<Vec<_>, _>>()?;
    let properties = decode_map_field(value, "properties", checkpoint)?;
    Ok(Value::Node(NodeValue {
        element_id: required_string(value, "elementId")?,
        labels,
        properties,
    }))
}

fn decode_relationship(
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    Ok(Value::Relationship(RelationshipValue {
        element_id: required_string(value, "elementId")?,
        relationship_type: required_string(value, "type")?,
        start: required_string(value, "start")?,
        end: required_string(value, "end")?,
        properties: decode_map_field(value, "properties", checkpoint)?,
    }))
}

fn decode_path(
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    let nodes = required_array(value, "nodes")?
        .iter()
        .map(|value| match decode_json_internal(value, checkpoint)? {
            Value::Node(value) => Ok(value),
            _ => Err(ValueError::new("Path nodes must use tagged Node values")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let relationships = required_array(value, "relationships")?
        .iter()
        .map(|value| match decode_json_internal(value, checkpoint)? {
            Value::Relationship(value) => Ok(value),
            _ => Err(ValueError::new(
                "Path relationships must use tagged Relationship values",
            )),
        })
        .collect::<Result<Vec<_>, _>>()?;
    if nodes.len() != relationships.len() + 1 {
        return Err(ValueError::new(
            "Path requires exactly one more node than relationship",
        ));
    }
    Ok(Value::Path(PathValue {
        nodes,
        relationships,
    }))
}

fn decode_point(
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    let coordinates = required_array(value, "coordinates")?
        .iter()
        .map(|value| {
            checkpoint(1)?;
            value
                .as_f64()
                .filter(|value| value.is_finite())
                .ok_or_else(|| ValueError::new("Point coordinates must be finite JSON numbers"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    Ok(Value::Point(PointValue::new(
        &required_string(value, "crs")?,
        coordinates,
    )?))
}

fn decode_vector(
    value: &Map<String, JsonValue>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Value, ValueError> {
    let coordinate_type = VectorCoordinateType::parse(&required_string(value, "coordinateType")?)
        .ok_or_else(|| ValueError::new("Vector coordinateType is unsupported"))?;
    let dimension = required_usize(value, "dimension")?;
    let values = required_array(value, "values")?;
    if values.len() != dimension {
        return Err(ValueError::new(
            "Vector values length does not match dimension",
        ));
    }
    let values = decode_vector_values(coordinate_type, values, checkpoint)?;
    Ok(Value::Vector(VectorValue::new_with_checkpoint(
        coordinate_type,
        values,
        checkpoint,
    )?))
}

fn decode_vector_values(
    coordinate_type: VectorCoordinateType,
    values: &[JsonValue],
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<VectorValues, ValueError> {
    match coordinate_type {
        VectorCoordinateType::I8 => {
            integer_vector(values, i8::try_from, checkpoint).map(VectorValues::I8)
        }
        VectorCoordinateType::I16 => {
            integer_vector(values, i16::try_from, checkpoint).map(VectorValues::I16)
        }
        VectorCoordinateType::I32 => {
            integer_vector(values, i32::try_from, checkpoint).map(VectorValues::I32)
        }
        VectorCoordinateType::I64 => values
            .iter()
            .map(|value| decode_vector_i64(value, checkpoint))
            .collect::<Result<Vec<_>, _>>()
            .map(VectorValues::I64),
        VectorCoordinateType::F32 => float_vector(
            values,
            |value| {
                let converted = value as f32;
                converted.is_finite().then_some(converted).ok_or_else(|| {
                    ValueError::new("Vector FLOAT32 coordinate is outside finite range")
                })
            },
            checkpoint,
        )
        .map(VectorValues::F32),
        VectorCoordinateType::F64 => float_vector(
            values,
            |value| {
                value
                    .is_finite()
                    .then_some(value)
                    .ok_or_else(|| ValueError::new("Vector FLOAT64 coordinate must be finite"))
            },
            checkpoint,
        )
        .map(VectorValues::F64),
    }
}

fn integer_vector<T>(
    values: &[JsonValue],
    convert: impl Fn(i64) -> Result<T, std::num::TryFromIntError>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Vec<T>, ValueError> {
    values
        .iter()
        .map(|value| decode_vector_i64(value, checkpoint))
        .map(|value| {
            value.and_then(|value| {
                convert(value).map_err(|_| {
                    ValueError::new("Vector integer coordinate is outside coordinateType range")
                })
            })
        })
        .collect()
}

fn decode_vector_i64(
    value: &JsonValue,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<i64, ValueError> {
    match decode_json_internal(value, checkpoint)? {
        Value::Integer(value) => Ok(value),
        _ => Err(ValueError::new(
            "Vector integer coordinate must be an Integer",
        )),
    }
}

fn float_vector<T>(
    values: &[JsonValue],
    convert: impl Fn(f64) -> Result<T, ValueError>,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<Vec<T>, ValueError> {
    values
        .iter()
        .map(|value| {
            let value = match decode_json_internal(value, checkpoint)? {
                Value::Integer(value) => value as f64,
                Value::Float(value) => value,
                _ => return Err(ValueError::new("Vector float coordinate must be numeric")),
            };
            convert(value)
        })
        .collect()
}

fn object_json<const N: usize>(fields: [(&str, JsonValue); N]) -> JsonValue {
    JsonValue::Object(
        fields
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value))
            .collect(),
    )
}

fn checked_text<E>(
    text: &str,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
) -> Result<String, E> {
    checkpoint(1, 0)?;
    if text.len() >= 64 * 1024 {
        checkpoint(0, 0)?;
    }
    let mut result = String::with_capacity(text.len());
    let mut position = 0;
    while position < text.len() {
        let mut end = position.saturating_add(64 * 1024).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        checkpoint(0, (end - position) as u64)?;
        result.push_str(&text[position..end]);
        position = end;
    }
    if text.len() >= 64 * 1024 {
        checkpoint(0, 0)?;
    }
    Ok(result)
}

fn checked_string<E>(
    text: &str,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
) -> Result<JsonValue, E> {
    checked_text(text, checkpoint).map(JsonValue::String)
}

fn tagged_text_checked<E>(
    tag: &str,
    value: &str,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
) -> Result<JsonValue, E> {
    Ok(object_json([
        ("$type", JsonValue::String(tag.to_owned())),
        ("value", checked_string(value, checkpoint)?),
    ]))
}

fn map_json_checked<E>(
    values: &BTreeMap<String, Value>,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
    allow_property: &dyn Fn(&str) -> bool,
    filter_keys: bool,
) -> Result<JsonValue, E> {
    let mut object = Map::new();
    for (key, value) in values {
        checkpoint(1, 0)?;
        if filter_keys && !allow_property(key) {
            continue;
        }
        object.insert(
            checked_text(key, checkpoint)?,
            encode_json_filtered_checked(value, checkpoint, allow_property)?,
        );
    }
    Ok(JsonValue::Object(object))
}

fn encode_node_checked<E>(
    value: &NodeValue,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
    allow_property: &dyn Fn(&str) -> bool,
) -> Result<JsonValue, E> {
    let labels = JsonValue::Array(
        value
            .labels
            .iter()
            .map(|label| checked_string(label, checkpoint))
            .collect::<Result<_, _>>()?,
    );
    graph_json_checked(
        "Node",
        &value.element_id,
        &value.properties,
        [("labels", labels)],
        checkpoint,
        allow_property,
    )
}

fn encode_relationship_checked<E>(
    value: &RelationshipValue,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
    allow_property: &dyn Fn(&str) -> bool,
) -> Result<JsonValue, E> {
    let endpoints = [
        (
            "type",
            checked_string(&value.relationship_type, checkpoint)?,
        ),
        ("start", checked_string(&value.start, checkpoint)?),
        ("end", checked_string(&value.end, checkpoint)?),
    ];
    graph_json_checked(
        "Relationship",
        &value.element_id,
        &value.properties,
        endpoints,
        checkpoint,
        allow_property,
    )
}

fn graph_json_checked<E, const N: usize>(
    tag: &str,
    element_id: &str,
    properties: &BTreeMap<String, Value>,
    extra: [(&str, JsonValue); N],
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
    allow_property: &dyn Fn(&str) -> bool,
) -> Result<JsonValue, E> {
    checkpoint(1, 0)?;
    let mut fields: Map<String, JsonValue> = [
        ("$type".to_owned(), JsonValue::String(tag.to_owned())),
        (
            "elementId".to_owned(),
            checked_string(element_id, checkpoint)?,
        ),
        (
            "properties".to_owned(),
            map_json_checked(properties, checkpoint, allow_property, true)?,
        ),
    ]
    .into_iter()
    .collect();
    fields.extend(
        extra
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    );
    Ok(JsonValue::Object(fields))
}

fn encode_vector_checked<E>(
    value: &VectorValue,
    checkpoint: &mut impl FnMut(u64, u64) -> Result<(), E>,
    allow_property: &dyn Fn(&str) -> bool,
) -> Result<JsonValue, E> {
    macro_rules! values {
        ($values:expr, $convert:expr) => {{
            $values
                .iter()
                .map(|value| {
                    encode_json_filtered_checked(&$convert(*value), checkpoint, allow_property)
                })
                .collect::<Result<Vec<_>, _>>()?
        }};
    }
    let values = match value.values() {
        VectorValues::I8(values) => values!(values, |value| Value::Integer(i64::from(value))),
        VectorValues::I16(values) => values!(values, |value| Value::Integer(i64::from(value))),
        VectorValues::I32(values) => values!(values, |value| Value::Integer(i64::from(value))),
        VectorValues::I64(values) => values!(values, Value::Integer),
        VectorValues::F32(values) => values!(values, |value| Value::Float(f64::from(value))),
        VectorValues::F64(values) => values!(values, Value::Float),
    };
    Ok(object_json([
        ("$type", JsonValue::String("Vector".to_owned())),
        (
            "coordinateType",
            JsonValue::String(value.coordinate_type().as_str().to_owned()),
        ),
        ("dimension", JsonValue::from(value.dimension())),
        ("values", JsonValue::Array(values)),
    ]))
}

fn decode_map_field(
    value: &Map<String, JsonValue>,
    field: &str,
    checkpoint: &mut DecodeCheckpoint<'_>,
) -> Result<BTreeMap<String, Value>, ValueError> {
    let object = value
        .get(field)
        .and_then(JsonValue::as_object)
        .ok_or_else(|| ValueError::new(format!("{field} must be an object")))?;
    match decode_plain_map(object, checkpoint)? {
        Value::Map(value) => Ok(value),
        _ => Err(ValueError::new(format!("{field} must be an object"))),
    }
}

fn required_text_ref<'a>(
    value: &'a Map<String, JsonValue>,
    field: &str,
) -> Result<&'a str, ValueError> {
    value
        .get(field)
        .and_then(JsonValue::as_str)
        .ok_or_else(|| ValueError::new(format!("{field} must be a string")))
}

fn required_string(value: &Map<String, JsonValue>, field: &str) -> Result<String, ValueError> {
    value
        .get(field)
        .and_then(JsonValue::as_str)
        .map(str::to_owned)
        .ok_or_else(|| ValueError::new(format!("{field} must be a string")))
}

fn required_array<'a>(
    value: &'a Map<String, JsonValue>,
    field: &str,
) -> Result<&'a Vec<JsonValue>, ValueError> {
    value
        .get(field)
        .and_then(JsonValue::as_array)
        .ok_or_else(|| ValueError::new(format!("{field} must be an array")))
}

fn required_usize(value: &Map<String, JsonValue>, field: &str) -> Result<usize, ValueError> {
    let value = value
        .get(field)
        .and_then(JsonValue::as_u64)
        .ok_or_else(|| ValueError::new(format!("{field} must be a non-negative integer")))?;
    usize::try_from(value).map_err(|_| ValueError::new(format!("{field} exceeds addressable size")))
}

fn require_exact_fields(
    value: &Map<String, JsonValue>,
    allowed: &[&str],
) -> Result<(), ValueError> {
    if let Some(field) = value
        .keys()
        .find(|field| !allowed.contains(&field.as_str()))
    {
        return Err(ValueError::new(format!(
            "tagged Lithograph JSON contains unexpected field {field:?}"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

    use super::*;
    use crate::query::QueryErrorKind;
    use crate::query::resources::{ExecutionControl, QueryResources};

    #[test]
    fn vector_decode_cancels_within_256_final_finite_checks() {
        const DIMENSION: usize = 4096;
        const DECODE_CHECKS: usize = DIMENSION / 256;
        for coordinate_type in ["FLOAT32", "FLOAT64"] {
            let raw = json!({
                "$type": "Vector",
                "coordinateType": coordinate_type,
                "dimension": DIMENSION,
                "values": vec![1.0; DIMENSION],
            });
            let checks = Cell::new(0);
            let interrupt = || {
                checks.set(checks.get() + 1);
                checks.get() > DECODE_CHECKS
            };
            let resources = QueryResources::default();
            let control = ExecutionControl::new(&resources, &interrupt);
            let error = decode_json_checked(&raw, &mut |units| control.work(units as u64))
                .expect_err("cancel during the final finite scan");
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
            assert_eq!(checks.get(), DECODE_CHECKS + 1);
        }
    }
}
