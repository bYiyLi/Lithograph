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

pub fn decode_json(value: &JsonValue) -> Result<Value, ValueError> {
    match value {
        JsonValue::Null => Ok(Value::Null),
        JsonValue::Bool(value) => Ok(Value::Boolean(*value)),
        JsonValue::Number(value) => decode_number(value),
        JsonValue::String(value) => Ok(Value::String(value.clone())),
        JsonValue::Array(values) => values
            .iter()
            .map(decode_json)
            .collect::<Result<Vec<_>, _>>()
            .map(Value::List),
        JsonValue::Object(value) => decode_object(value),
    }
}

pub fn encode_json(value: &Value) -> JsonValue {
    match value {
        Value::Null => JsonValue::Null,
        Value::Boolean(value) => JsonValue::Bool(*value),
        Value::Integer(value) if (-JS_SAFE_INTEGER..=JS_SAFE_INTEGER).contains(value) => {
            JsonValue::Number((*value).into())
        }
        Value::Integer(value) => json!({"$type":"Integer","value":value.to_string()}),
        Value::Float(value) if value.is_finite() => Number::from_f64(*value)
            .map(JsonValue::Number)
            .unwrap_or_else(|| tagged_float(*value)),
        Value::Float(value) => tagged_float(*value),
        Value::String(value) => JsonValue::String(value.clone()),
        Value::List(values) => JsonValue::Array(values.iter().map(encode_json).collect()),
        Value::Map(values) => encode_map(values),
        Value::Node(value) => encode_node(value),
        Value::Relationship(value) => encode_relationship(value),
        Value::Path(value) => encode_path(value),
        Value::Date(value) => tagged_text("Date", value.as_str()),
        Value::LocalTime(value) => tagged_text("LocalTime", value.as_str()),
        Value::Time(value) => tagged_text("Time", value.as_str()),
        Value::LocalDateTime(value) => tagged_text("LocalDateTime", value.as_str()),
        Value::ZonedDateTime(value) => {
            json!({"$type":"ZonedDateTime","value":value.value(),"zone":value.zone()})
        }
        Value::Duration(value) => tagged_text("Duration", value.as_str()),
        Value::Point(value) => json!({
            "$type":"Point",
            "crs":value.crs(),
            "coordinates":value.coordinates(),
        }),
        Value::Vector(value) => encode_vector(value),
        Value::Uuid(value) => tagged_text("UUID", &value.to_canonical()),
    }
}

pub fn decode_parameters(value: &JsonValue) -> Result<BTreeMap<String, Value>, ValueError> {
    let object = value
        .as_object()
        .ok_or_else(|| ValueError::new("Cypher parameters must be a JSON object"))?;
    object
        .iter()
        .map(|(name, raw)| {
            let value = decode_json(raw)?;
            if !value.is_parameter_value() {
                return Err(ValueError::new(format!(
                    "parameter {name:?} uses a structural value that cannot be supplied as a Cypher parameter"
                )));
            }
            Ok((name.clone(), value))
        })
        .collect()
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

fn decode_object(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    let Some(tag) = value.get("$type") else {
        return decode_plain_map(value);
    };
    let tag = tag
        .as_str()
        .ok_or_else(|| ValueError::new("$type must be a string"))?;
    if let Some(decoded) = decode_numeric_or_map_tag(tag, value)? {
        return Ok(decoded);
    }
    if let Some(decoded) = decode_structural_tag(tag, value)? {
        return Ok(decoded);
    }
    if let Some(decoded) = decode_basic_temporal_tag(tag, value)? {
        return Ok(decoded);
    }
    if let Some(decoded) = decode_datetime_duration_tag(tag, value)? {
        return Ok(decoded);
    }
    if let Some(decoded) = decode_spatial_vector_uuid_tag(tag, value)? {
        return Ok(decoded);
    }
    Err(ValueError::new(format!(
        "unknown Lithograph JSON $type {tag:?}"
    )))
}

fn decode_numeric_or_map_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
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
            decode_wrapped_map(value).map(Some)
        }
        _ => Ok(None),
    }
}

fn decode_structural_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
) -> Result<Option<Value>, ValueError> {
    match tag {
        "Node" => {
            require_exact_fields(value, &["$type", "elementId", "labels", "properties"])?;
            decode_node(value).map(Some)
        }
        "Relationship" => {
            require_exact_fields(
                value,
                &["$type", "elementId", "type", "start", "end", "properties"],
            )?;
            decode_relationship(value).map(Some)
        }
        "Path" => {
            require_exact_fields(value, &["$type", "nodes", "relationships"])?;
            decode_path(value).map(Some)
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
            Ok(Some(Value::Time(TimeValue::parse(&required_string(
                value, "value",
            )?)?)))
        }
        _ => Ok(None),
    }
}

fn decode_datetime_duration_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
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
            Ok(Some(Value::Duration(DurationValue::parse(
                &required_string(value, "value")?,
            )?)))
        }
        _ => Ok(None),
    }
}

fn decode_spatial_vector_uuid_tag(
    tag: &str,
    value: &Map<String, JsonValue>,
) -> Result<Option<Value>, ValueError> {
    match tag {
        "Point" => {
            require_exact_fields(value, &["$type", "crs", "coordinates"])?;
            decode_point(value).map(Some)
        }
        "Vector" => {
            require_exact_fields(value, &["$type", "coordinateType", "dimension", "values"])?;
            decode_vector(value).map(Some)
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

fn decode_plain_map(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    value
        .iter()
        .map(|(key, value)| Ok((key.clone(), decode_json(value)?)))
        .collect::<Result<BTreeMap<_, _>, ValueError>>()
        .map(Value::Map)
}

fn decode_wrapped_map(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    let entries = value
        .get("entries")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| ValueError::new("Map wrapper requires an object-valued entries field"))?;
    decode_plain_map(entries)
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

fn decode_node(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    let labels = required_array(value, "labels")?
        .iter()
        .map(|value| {
            value
                .as_str()
                .map(str::to_owned)
                .ok_or_else(|| ValueError::new("Node labels must be strings"))
        })
        .collect::<Result<Vec<_>, _>>()?;
    let properties = decode_map_field(value, "properties")?;
    Ok(Value::Node(NodeValue {
        element_id: required_string(value, "elementId")?,
        labels,
        properties,
    }))
}

fn decode_relationship(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    Ok(Value::Relationship(RelationshipValue {
        element_id: required_string(value, "elementId")?,
        relationship_type: required_string(value, "type")?,
        start: required_string(value, "start")?,
        end: required_string(value, "end")?,
        properties: decode_map_field(value, "properties")?,
    }))
}

fn decode_path(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    let nodes = required_array(value, "nodes")?
        .iter()
        .map(|value| match decode_json(value)? {
            Value::Node(value) => Ok(value),
            _ => Err(ValueError::new("Path nodes must use tagged Node values")),
        })
        .collect::<Result<Vec<_>, _>>()?;
    let relationships = required_array(value, "relationships")?
        .iter()
        .map(|value| match decode_json(value)? {
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

fn decode_point(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    let coordinates = required_array(value, "coordinates")?
        .iter()
        .map(|value| {
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

fn decode_vector(value: &Map<String, JsonValue>) -> Result<Value, ValueError> {
    let coordinate_type = VectorCoordinateType::parse(&required_string(value, "coordinateType")?)
        .ok_or_else(|| ValueError::new("Vector coordinateType is unsupported"))?;
    let dimension = required_usize(value, "dimension")?;
    let values = required_array(value, "values")?;
    if values.len() != dimension {
        return Err(ValueError::new(
            "Vector values length does not match dimension",
        ));
    }
    let values = decode_vector_values(coordinate_type, values)?;
    Ok(Value::Vector(VectorValue::new(coordinate_type, values)?))
}

fn decode_vector_values(
    coordinate_type: VectorCoordinateType,
    values: &[JsonValue],
) -> Result<VectorValues, ValueError> {
    match coordinate_type {
        VectorCoordinateType::I8 => integer_vector(values, i8::try_from).map(VectorValues::I8),
        VectorCoordinateType::I16 => integer_vector(values, i16::try_from).map(VectorValues::I16),
        VectorCoordinateType::I32 => integer_vector(values, i32::try_from).map(VectorValues::I32),
        VectorCoordinateType::I64 => values
            .iter()
            .map(decode_vector_i64)
            .collect::<Result<Vec<_>, _>>()
            .map(VectorValues::I64),
        VectorCoordinateType::F32 => float_vector(values, |value| {
            let converted = value as f32;
            converted
                .is_finite()
                .then_some(converted)
                .ok_or_else(|| ValueError::new("Vector FLOAT32 coordinate is outside finite range"))
        })
        .map(VectorValues::F32),
        VectorCoordinateType::F64 => float_vector(values, |value| {
            value
                .is_finite()
                .then_some(value)
                .ok_or_else(|| ValueError::new("Vector FLOAT64 coordinate must be finite"))
        })
        .map(VectorValues::F64),
    }
}

fn integer_vector<T>(
    values: &[JsonValue],
    convert: impl Fn(i64) -> Result<T, std::num::TryFromIntError>,
) -> Result<Vec<T>, ValueError> {
    values
        .iter()
        .map(decode_vector_i64)
        .map(|value| {
            value.and_then(|value| {
                convert(value).map_err(|_| {
                    ValueError::new("Vector integer coordinate is outside coordinateType range")
                })
            })
        })
        .collect()
}

fn decode_vector_i64(value: &JsonValue) -> Result<i64, ValueError> {
    match decode_json(value)? {
        Value::Integer(value) => Ok(value),
        _ => Err(ValueError::new(
            "Vector integer coordinate must be an Integer",
        )),
    }
}

fn float_vector<T>(
    values: &[JsonValue],
    convert: impl Fn(f64) -> Result<T, ValueError>,
) -> Result<Vec<T>, ValueError> {
    values
        .iter()
        .map(|value| {
            let value = match decode_json(value)? {
                Value::Integer(value) => value as f64,
                Value::Float(value) => value,
                _ => return Err(ValueError::new("Vector float coordinate must be numeric")),
            };
            convert(value)
        })
        .collect()
}

fn encode_map(values: &BTreeMap<String, Value>) -> JsonValue {
    let entries = values
        .iter()
        .map(|(key, value)| (key.clone(), encode_json(value)))
        .collect::<Map<_, _>>();
    if values.contains_key("$type") {
        json!({"$type":"Map","entries":entries})
    } else {
        JsonValue::Object(entries)
    }
}

fn encode_node(value: &NodeValue) -> JsonValue {
    json!({
        "$type":"Node",
        "elementId":value.element_id,
        "labels":value.labels,
        "properties":map_json(&value.properties),
    })
}

fn encode_relationship(value: &RelationshipValue) -> JsonValue {
    json!({
        "$type":"Relationship",
        "elementId":value.element_id,
        "type":value.relationship_type,
        "start":value.start,
        "end":value.end,
        "properties":map_json(&value.properties),
    })
}

fn encode_path(value: &PathValue) -> JsonValue {
    json!({
        "$type":"Path",
        "nodes":value.nodes.iter().map(encode_node).collect::<Vec<_>>(),
        "relationships":value.relationships.iter().map(encode_relationship).collect::<Vec<_>>(),
    })
}

fn encode_vector(value: &VectorValue) -> JsonValue {
    let values = match value.values() {
        VectorValues::I8(values) => values
            .iter()
            .map(|value| JsonValue::from(*value))
            .collect::<Vec<_>>(),
        VectorValues::I16(values) => values
            .iter()
            .map(|value| JsonValue::from(*value))
            .collect::<Vec<_>>(),
        VectorValues::I32(values) => values
            .iter()
            .map(|value| JsonValue::from(*value))
            .collect::<Vec<_>>(),
        VectorValues::I64(values) => values
            .iter()
            .map(|value| encode_json(&Value::Integer(*value)))
            .collect(),
        VectorValues::F32(values) => values
            .iter()
            .map(|value| encode_json(&Value::Float(f64::from(*value))))
            .collect(),
        VectorValues::F64(values) => values
            .iter()
            .map(|value| encode_json(&Value::Float(*value)))
            .collect(),
    };
    json!({
        "$type":"Vector",
        "coordinateType":value.coordinate_type().as_str(),
        "dimension":value.dimension(),
        "values":values,
    })
}

fn tagged_float(value: f64) -> JsonValue {
    let text = if value.is_nan() {
        "NaN"
    } else if value.is_sign_positive() {
        "Infinity"
    } else {
        "-Infinity"
    };
    tagged_text("Float", text)
}

fn tagged_text(tag: &str, value: &str) -> JsonValue {
    json!({"$type":tag,"value":value})
}

fn map_json(values: &BTreeMap<String, Value>) -> JsonValue {
    values
        .iter()
        .map(|(key, value)| (key.clone(), encode_json(value)))
        .collect::<Map<_, _>>()
        .into()
}

fn decode_map_field(
    value: &Map<String, JsonValue>,
    field: &str,
) -> Result<BTreeMap<String, Value>, ValueError> {
    let object = value
        .get(field)
        .and_then(JsonValue::as_object)
        .ok_or_else(|| ValueError::new(format!("{field} must be an object")))?;
    match decode_plain_map(object)? {
        Value::Map(value) => Ok(value),
        _ => Err(ValueError::new(format!("{field} must be an object"))),
    }
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
