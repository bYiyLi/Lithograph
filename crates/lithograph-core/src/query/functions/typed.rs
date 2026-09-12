use crate::cypher::{
    DurationValue, PointValue, UuidValue, Value, VectorCoordinateType, VectorValue, VectorValues,
};

use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{contains_null, require_arity};

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let result = match name {
        "duration" => duration(values),
        "uuid" => uuid(values),
        "randomuuid" => random_uuid(values).map(|value| match value {
            Value::Uuid(value) => Value::String(value.to_canonical()),
            value => value,
        }),
        "uuid.leastsignificantbits" => uuid_bits(values, false),
        "uuid.mostsignificantbits" => uuid_bits(values, true),
        "point" => point(values),
        "point.distance" => point_distance(values),
        "point.withinbbox" => point_within_bbox(values),
        "vector" => vector(values),
        "vector.similarity.cosine" => vector_similarity(values, true),
        "vector.similarity.euclidean" => vector_similarity(values, false),
        "vector_dimension_count" => vector_dimension(values),
        "vector_distance" => vector_distance(values),
        "vector_norm" => vector_norm(values),
        _ => return None,
    };
    Some(result)
}

fn duration(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 2)?;
    if values.len() == 2 {
        return match (&values[0], &values[1]) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::String(input), Value::String(pattern)) => {
                duration_from_pattern(input, pattern).map(Value::Duration)
            }
            _ => temporal_type_error("duration pattern parsing requires two String values"),
        };
    }
    match &values[0] {
        Value::Duration(value) => Ok(Value::Duration(value.clone())),
        Value::String(value) => DurationValue::parse(value)
            .map(Value::Duration)
            .map_err(Into::into),
        Value::Map(value) => duration_from_map(value).map(Value::Duration),
        Value::Null => Ok(Value::Null),
        _ => temporal_type_error("duration"),
    }
}

fn duration_from_map(
    map: &std::collections::BTreeMap<String, Value>,
) -> QueryResult<DurationValue> {
    const FIELDS: &[&str] = &[
        "years",
        "months",
        "weeks",
        "days",
        "hours",
        "minutes",
        "seconds",
        "milliseconds",
        "microseconds",
        "nanoseconds",
    ];
    if map.is_empty() {
        return Err(QueryError::semantic(
            "duration() map must contain a component",
        ));
    }
    if let Some(name) = map.keys().find(|name| !FIELDS.contains(&name.as_str())) {
        return Err(QueryError::semantic(format!(
            "duration() map contains unsupported field {name}"
        )));
    }
    let months = number_field(map, "years")? * 12.0 + number_field(map, "months")?;
    let days = number_field(map, "weeks")? * 7.0 + number_field(map, "days")?;
    let seconds = number_field(map, "hours")? * 3_600.0
        + number_field(map, "minutes")? * 60.0
        + number_field(map, "seconds")?
        + number_field(map, "milliseconds")? / 1_000.0
        + number_field(map, "microseconds")? / 1_000_000.0
        + number_field(map, "nanoseconds")? / 1_000_000_000.0;
    duration_from_decimal_groups(months, days, seconds)
}

fn duration_from_decimal_groups(
    months: f64,
    days: f64,
    seconds: f64,
) -> QueryResult<DurationValue> {
    DurationValue::from_decimal_groups(months, days, seconds).map_err(Into::into)
}

fn number_field(map: &std::collections::BTreeMap<String, Value>, name: &str) -> QueryResult<f64> {
    match map.get(name) {
        None => Ok(0.0),
        Some(Value::Integer(value)) => Ok(*value as f64),
        Some(Value::Float(value)) => Ok(*value),
        Some(_) => temporal_type_error(&format!("duration field {name} must be numeric")),
    }
}

fn duration_from_pattern(input: &str, pattern: &str) -> QueryResult<DurationValue> {
    let (regular_expression, units) = duration_pattern_regex(pattern)?;
    let regex = regex::Regex::new(&regular_expression)
        .map_err(|error| QueryError::internal(format!("duration pattern regex failed: {error}")))?;
    let captures = regex.captures(input).ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::Type,
            "duration input does not match its pattern",
        )
    })?;
    let mut months = 0.0;
    let mut days = 0.0;
    let mut seconds = 0.0;
    for (index, unit) in units.into_iter().enumerate() {
        let value = captures[index + 1].parse::<f64>().map_err(|_| {
            QueryError::new(QueryErrorKind::Type, "duration pattern value is invalid")
        })?;
        match unit {
            'y' | 'Y' | 'u' => months += value * 12.0,
            'q' | 'Q' => months += value * 3.0,
            'M' | 'L' => months += value,
            'w' | 'W' => days += value * 7.0,
            'd' | 'D' => days += value,
            'h' | 'H' | 'k' | 'K' => seconds += value * 3_600.0,
            'm' => seconds += value * 60.0,
            's' => seconds += value,
            'A' => seconds += value / 1_000.0,
            'n' | 'S' | 'N' => seconds += value / 1_000_000_000.0,
            _ => return Err(QueryError::internal("invalid duration pattern unit")),
        }
    }
    duration_from_decimal_groups(months, days, seconds)
}

fn duration_pattern_regex(pattern: &str) -> QueryResult<(String, Vec<char>)> {
    let characters = pattern.chars().collect::<Vec<_>>();
    let mut regex = String::from("^");
    let mut units = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if characters[index] == '\'' {
            let (literal, next) = duration_pattern_literal(&characters, index)?;
            regex.push_str(&regex::escape(&literal));
            index = next;
            continue;
        }
        let character = characters[index];
        let start = index;
        while index < characters.len() && characters[index] == character {
            index += 1;
        }
        if character.is_ascii_alphabetic() {
            if !matches!(
                character,
                'y' | 'Y'
                    | 'u'
                    | 'q'
                    | 'Q'
                    | 'M'
                    | 'L'
                    | 'w'
                    | 'W'
                    | 'd'
                    | 'D'
                    | 'h'
                    | 'H'
                    | 'k'
                    | 'K'
                    | 'm'
                    | 's'
                    | 'A'
                    | 'n'
                    | 'S'
                    | 'N'
            ) {
                return Err(QueryError::semantic(format!(
                    "unsupported duration pattern component {character}"
                )));
            }
            regex.push_str("([+-]?(?:[0-9]+(?:\\.[0-9]*)?|\\.[0-9]+))");
            units.push(character);
        } else {
            regex.push_str(&regex::escape(&character.to_string().repeat(index - start)));
        }
    }
    regex.push('$');
    Ok((regex, units))
}

fn duration_pattern_literal(characters: &[char], start: usize) -> QueryResult<(String, usize)> {
    let mut literal = String::new();
    let mut index = start + 1;
    while let Some(character) = characters.get(index) {
        if *character == '\'' {
            return Ok((literal, index + 1));
        }
        literal.push(*character);
        index += 1;
    }
    Err(QueryError::semantic(
        "unterminated duration pattern literal",
    ))
}

fn uuid(values: &[Value]) -> QueryResult<Value> {
    match values {
        [] => random_uuid(values),
        [Value::String(value)] => UuidValue::parse(value).map(Value::Uuid).map_err(Into::into),
        [Value::Integer(most), Value::Integer(least)] => {
            let mut bytes = [0_u8; 16];
            bytes[..8].copy_from_slice(&most.to_be_bytes());
            bytes[8..].copy_from_slice(&least.to_be_bytes());
            Ok(Value::Uuid(UuidValue::from_bytes(bytes)))
        }
        [Value::Null] | [Value::Null, _] | [_, Value::Null] => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "uuid() expects no argument, String, or two Integers",
        )),
    }
}

fn random_uuid(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 0, 0)?;
    let mut bytes = [0_u8; 16];
    getrandom::fill(&mut bytes)
        .map_err(|error| QueryError::internal(format!("random source failed: {error}")))?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Ok(Value::Uuid(UuidValue::from_bytes(bytes)))
}

fn uuid_bits(values: &[Value], most: bool) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::Uuid(value) => {
            let mut bytes = [0_u8; 8];
            if most {
                bytes.copy_from_slice(&value.as_bytes()[..8]);
            } else {
                bytes.copy_from_slice(&value.as_bytes()[8..]);
            }
            Ok(Value::Integer(i64::from_be_bytes(bytes)))
        }
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "UUID bit function requires UUID",
        )),
    }
}

fn point(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    let Value::Map(map) = &values[0] else {
        if matches!(values[0], Value::Null) {
            return Ok(Value::Null);
        }
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "point() requires Map",
        ));
    };
    let geographic = map.contains_key("longitude") || map.contains_key("latitude");
    let first = number(map.get(if geographic { "longitude" } else { "x" }))?;
    let second = number(map.get(if geographic { "latitude" } else { "y" }))?;
    let third = map
        .get("height")
        .or_else(|| map.get("z"))
        .map(number_value)
        .transpose()?;
    let crs = point_crs(map, geographic, third.is_some())?;
    let mut coordinates = vec![first, second];
    if let Some(third) = third {
        coordinates.push(third);
    }
    PointValue::new(&crs, coordinates)
        .map(Value::Point)
        .map_err(Into::into)
}

fn point_crs(
    map: &std::collections::BTreeMap<String, Value>,
    geographic: bool,
    three_dimensional: bool,
) -> QueryResult<String> {
    let named = match map.get("crs") {
        Some(Value::String(value)) => Some(value.to_ascii_lowercase()),
        Some(_) => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "point crs must be String",
            ));
        }
        None => None,
    };
    let numbered = match map.get("srid") {
        Some(Value::Integer(4_326)) => Some("wgs-84"),
        Some(Value::Integer(4_979)) => Some("wgs-84-3d"),
        Some(Value::Integer(7_203)) => Some("cartesian"),
        Some(Value::Integer(9_157)) => Some("cartesian-3d"),
        Some(Value::Integer(_)) => return temporal_type_error("point srid is unsupported"),
        Some(_) => return temporal_type_error("point srid must be Integer"),
        None => None,
    };
    if let (Some(named), Some(numbered)) = (&named, numbered)
        && named != numbered
    {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "point crs and srid identify different coordinate systems",
        ));
    }
    Ok(named.unwrap_or_else(|| {
        numbered
            .map_or_else(
                || match (geographic, three_dimensional) {
                    (true, true) => "wgs-84-3d",
                    (true, false) => "wgs-84",
                    (false, true) => "cartesian-3d",
                    (false, false) => "cartesian",
                },
                |value| value,
            )
            .to_owned()
    }))
}

fn point_distance(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    let (Value::Point(left), Value::Point(right)) = (&values[0], &values[1]) else {
        if contains_null(values) {
            return Ok(Value::Null);
        }
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "point.distance() requires two Points",
        ));
    };
    if left.crs() != right.crs() {
        return Ok(Value::Null);
    }
    let distance = if left.crs().starts_with("wgs-84") {
        geographic_distance(left.coordinates(), right.coordinates())
    } else {
        euclidean(left.coordinates(), right.coordinates())
    };
    Ok(Value::Float(distance))
}

fn point_within_bbox(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 3, 3)?;
    let (Value::Point(point), Value::Point(lower), Value::Point(upper)) =
        (&values[0], &values[1], &values[2])
    else {
        if contains_null(values) {
            return Ok(Value::Null);
        }
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "point.withinBBox() requires three Points",
        ));
    };
    if point.crs() != lower.crs() || point.crs() != upper.crs() {
        return Ok(Value::Null);
    }
    Ok(Value::Boolean(
        point
            .coordinates()
            .iter()
            .zip(lower.coordinates())
            .zip(upper.coordinates())
            .all(|((value, lower), upper)| value >= lower && value <= upper),
    ))
}

fn vector(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 3, 3)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let Value::Integer(dimension) = values[1] else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "vector() dimension must be Integer",
        ));
    };
    let dimension = usize::try_from(dimension)
        .map_err(|_| QueryError::new(QueryErrorKind::Type, "vector dimension is negative"))?;
    let Value::String(coordinate_type) = &values[2] else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "vector() coordinate type must be a type name",
        ));
    };
    let coordinate_type = VectorCoordinateType::parse(coordinate_type)
        .ok_or_else(|| QueryError::semantic("vector coordinate type is unsupported"))?;
    let numbers = vector_input(&values[0])?;
    if numbers.len() != dimension {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "vector() input length differs from its dimension",
        ));
    }
    VectorValue::new(coordinate_type, cast_vector(coordinate_type, &numbers)?)
        .map(Value::Vector)
        .map_err(Into::into)
}

fn vector_similarity(values: &[Value], cosine: bool) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let left = similarity_vector(&values[0])?;
    let right = similarity_vector(&values[1])?;
    require_same_vector_dimension(&left, &right)?;
    if cosine {
        let similarity = cosine_similarity(&left, &right)?;
        Ok(Value::Float(f64::from(
            ((1.0 + similarity) / 2.0).clamp(0.0, 1.0),
        )))
    } else {
        let distance = euclidean_squared_f32(&left, &right);
        Ok(Value::Float(f64::from(1.0 / (1.0 + distance))))
    }
}

fn vector_distance(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 3, 3)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let (Value::Vector(left), Value::Vector(right), Value::String(metric)) =
        (&values[0], &values[1], &values[2])
    else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "vector_distance() requires two Vectors and a distance metric",
        ));
    };
    let left = vector_numbers_f32(left);
    let right = vector_numbers_f32(right);
    require_same_vector_dimension(&left, &right)?;
    let distance = match metric.to_ascii_uppercase().as_str() {
        "EUCLIDEAN" => euclidean_squared_f32(&left, &right).sqrt(),
        "EUCLIDEAN_SQUARED" => euclidean_squared_f32(&left, &right),
        "MANHATTAN" => left
            .iter()
            .zip(&right)
            .map(|(left, right)| (left - right).abs())
            .sum(),
        "COSINE" => 1.0 - cosine_similarity(&left, &right)?,
        "DOT" => -left
            .iter()
            .zip(&right)
            .map(|(left, right)| left * right)
            .sum::<f32>(),
        "HAMMING" => left
            .iter()
            .zip(&right)
            .filter(|(left, right)| left != right)
            .count() as f32,
        _ => return Err(QueryError::semantic("unsupported vector distance metric")),
    };
    Ok(Value::Float(f64::from(distance)))
}

fn vector_norm(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let (Value::Vector(value), Value::String(metric)) = (&values[0], &values[1]) else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "vector_norm() requires a Vector and a distance metric",
        ));
    };
    let values = vector_numbers_f32(value);
    let result = match metric.to_ascii_uppercase().as_str() {
        "EUCLIDEAN" => values.iter().map(|value| value * value).sum::<f32>().sqrt(),
        "MANHATTAN" => values.iter().map(|value| value.abs()).sum(),
        _ => {
            return Err(QueryError::semantic(
                "vector_norm() metric must be EUCLIDEAN or MANHATTAN",
            ));
        }
    };
    Ok(Value::Float(f64::from(result)))
}

fn vector_dimension(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 1, 1)?;
    match &values[0] {
        Value::Vector(value) => Ok(Value::Integer(value.dimension() as i64)),
        Value::Null => Ok(Value::Null),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "vector_dimension_count() requires Vector",
        )),
    }
}

fn similarity_vector(value: &Value) -> QueryResult<Vec<f32>> {
    let values = match value {
        Value::Vector(value) => vector_numbers_f32(value),
        Value::List(values) => values
            .iter()
            .map(|value| number_value(value).map(|value| value as f32))
            .collect::<QueryResult<Vec<_>>>()?,
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "vector similarity requires Vector or numeric List input",
            ));
        }
    };
    if values.is_empty() || values.iter().any(|value| !value.is_finite()) {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "vector similarity input must be non-empty and finite",
        ));
    }
    Ok(values)
}

fn require_same_vector_dimension(left: &[f32], right: &[f32]) -> QueryResult<()> {
    if left.len() == right.len() {
        Ok(())
    } else {
        Err(QueryError::new(
            QueryErrorKind::Type,
            "vector dimensions must match",
        ))
    }
}

fn cosine_similarity(left: &[f32], right: &[f32]) -> QueryResult<f32> {
    let dot = left
        .iter()
        .zip(right)
        .map(|(left, right)| left * right)
        .sum::<f32>();
    let denominator = left.iter().map(|value| value * value).sum::<f32>().sqrt()
        * right.iter().map(|value| value * value).sum::<f32>().sqrt();
    if denominator == 0.0 {
        Err(QueryError::new(
            QueryErrorKind::Type,
            "cosine vector input must have a non-zero norm",
        ))
    } else {
        Ok(dot / denominator)
    }
}

fn euclidean_squared_f32(left: &[f32], right: &[f32]) -> f32 {
    left.iter()
        .zip(right)
        .map(|(left, right)| (left - right).powi(2))
        .sum()
}

fn vector_input(value: &Value) -> QueryResult<Vec<f64>> {
    match value {
        Value::List(values) => values.iter().map(number_value).collect(),
        Value::String(value) => value
            .trim_matches(['[', ']'])
            .split(',')
            .map(|value| {
                value.trim().parse::<f64>().map_err(|_| {
                    QueryError::new(QueryErrorKind::Type, "vector String contains non-number")
                })
            })
            .collect(),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "vector() input must be List or String",
        )),
    }
}

fn cast_vector(kind: VectorCoordinateType, values: &[f64]) -> QueryResult<VectorValues> {
    let integer = |value: f64, minimum: f64, maximum: f64| {
        if value.is_finite() && value.fract() == 0.0 && (minimum..=maximum).contains(&value) {
            Ok(value as i64)
        } else {
            Err(QueryError::new(
                QueryErrorKind::Type,
                "vector coordinate cannot be represented by its declared type",
            ))
        }
    };
    Ok(match kind {
        VectorCoordinateType::I8 => VectorValues::I8(
            values
                .iter()
                .map(|value| integer(*value, i8::MIN as f64, i8::MAX as f64).map(|v| v as i8))
                .collect::<QueryResult<_>>()?,
        ),
        VectorCoordinateType::I16 => VectorValues::I16(
            values
                .iter()
                .map(|value| integer(*value, i16::MIN as f64, i16::MAX as f64).map(|v| v as i16))
                .collect::<QueryResult<_>>()?,
        ),
        VectorCoordinateType::I32 => VectorValues::I32(
            values
                .iter()
                .map(|value| integer(*value, i32::MIN as f64, i32::MAX as f64).map(|v| v as i32))
                .collect::<QueryResult<_>>()?,
        ),
        VectorCoordinateType::I64 => VectorValues::I64(
            values
                .iter()
                .map(|value| integer(*value, i64::MIN as f64, i64::MAX as f64))
                .collect::<QueryResult<_>>()?,
        ),
        VectorCoordinateType::F32 => {
            VectorValues::F32(values.iter().map(|value| *value as f32).collect())
        }
        VectorCoordinateType::F64 => VectorValues::F64(values.to_vec()),
    })
}

fn vector_numbers(value: &VectorValue) -> Vec<f64> {
    match value.values() {
        VectorValues::I8(values) => values.iter().map(|value| f64::from(*value)).collect(),
        VectorValues::I16(values) => values.iter().map(|value| f64::from(*value)).collect(),
        VectorValues::I32(values) => values.iter().map(|value| f64::from(*value)).collect(),
        VectorValues::I64(values) => values.iter().map(|value| *value as f64).collect(),
        VectorValues::F32(values) => values.iter().map(|value| f64::from(*value)).collect(),
        VectorValues::F64(values) => values.clone(),
    }
}

fn vector_numbers_f32(value: &VectorValue) -> Vec<f32> {
    vector_numbers(value)
        .into_iter()
        .map(|value| value as f32)
        .collect()
}

fn geographic_distance(left: &[f64], right: &[f64]) -> f64 {
    let longitude = (right[0] - left[0]).to_radians();
    let latitude = (right[1] - left[1]).to_radians();
    let a = (latitude / 2.0).sin().powi(2)
        + left[1].to_radians().cos()
            * right[1].to_radians().cos()
            * (longitude / 2.0).sin().powi(2);
    let surface = 6_371_000.0 * 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    if left.len() == 3 && right.len() == 3 {
        (surface.powi(2) + (right[2] - left[2]).powi(2)).sqrt()
    } else {
        surface
    }
}

fn euclidean(left: &[f64], right: &[f64]) -> f64 {
    left.iter()
        .zip(right)
        .map(|(left, right)| (left - right).powi(2))
        .sum::<f64>()
        .sqrt()
}

fn number(value: Option<&Value>) -> QueryResult<f64> {
    value.map_or_else(
        || Err(QueryError::semantic("point coordinate is missing")),
        number_value,
    )
}

fn number_value(value: &Value) -> QueryResult<f64> {
    match value {
        Value::Integer(value) => Ok(*value as f64),
        Value::Float(value) => Ok(*value),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "numeric value is required",
        )),
    }
}

fn temporal_type_error<T>(name: &str) -> QueryResult<T> {
    Err(QueryError::new(
        QueryErrorKind::Type,
        format!("{name}() input has an incompatible type"),
    ))
}
