use crate::cypher::{
    DurationValue, PointValue, UuidValue, Value, VectorCoordinateType, VectorValue, VectorValues,
};

use super::super::expression::value_bytes_with_control;
use super::super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, checked_mul,
};
use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{contains_null, require_arity};

super::legacy_evaluators!();

pub(super) fn evaluate_with_memory(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    const FUNCTIONS: &[&str] = &[
        "duration",
        "uuid",
        "randomuuid",
        "uuid.leastsignificantbits",
        "uuid.mostsignificantbits",
        "point",
        "point.distance",
        "point.withinbbox",
        "vector",
        "vector.similarity.cosine",
        "vector.similarity.euclidean",
        "vector_dimension_count",
        "vector_distance",
        "vector_norm",
    ];
    if !FUNCTIONS.contains(&name) {
        return None;
    }
    let result = (|| {
        let mut memory = control
            .map(|control| {
                typed_workspace_bytes(name, values, control)
                    .and_then(|bytes| control.reserve(bytes))
            })
            .transpose()?;
        if let Some(control) = control {
            control.check()?;
        }
        let result = evaluate_typed_value(name, values, control)?;
        if let Some(control) = control {
            control.check()?;
        }
        if let Some(memory) = &mut memory {
            let heap_bytes = value_bytes_with_control(&result, control)?
                .checked_sub(std::mem::size_of::<Value>() as u64)
                .ok_or_else(|| QueryError::internal("invalid result memory size"))?;
            memory.resize(heap_bytes)?;
        }
        *result_memory = memory;
        Ok(result)
    })();
    Some(result)
}

fn evaluate_typed_value(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    match name {
        "duration" => duration(values, control),
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
        _ => evaluate_vector_value(name, values, control),
    }
}

fn evaluate_vector_value(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    match name {
        "vector" => vector(values, control),
        "vector.similarity.cosine" => vector_similarity(values, true, control),
        "vector.similarity.euclidean" => vector_similarity(values, false, control),
        "vector_dimension_count" => vector_dimension(values),
        "vector_distance" => vector_distance(values, control),
        "vector_norm" => vector_norm(values, control),
        _ => Err(QueryError::internal("unknown typed function")),
    }
}

fn duration(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
    require_arity(values, 1, 2)?;
    if values.len() == 2 {
        return match (&values[0], &values[1]) {
            (Value::Null, _) | (_, Value::Null) => Ok(Value::Null),
            (Value::String(input), Value::String(pattern)) => {
                duration_from_pattern(input, pattern, control).map(Value::Duration)
            }
            _ => temporal_type_error("duration pattern parsing requires two String values"),
        };
    }
    match &values[0] {
        Value::Duration(value) => Ok(Value::Duration(value.clone())),
        Value::String(value) => {
            DurationValue::parse_with_checkpoint(value, |bytes| match control {
                Some(control) if bytes == 0 => control.check(),
                Some(control) => control.bytes(bytes as u64),
                None => Ok(()),
            })
            .map(Value::Duration)
        }
        Value::Map(value) => duration_from_map(value, control).map(Value::Duration),
        Value::Null => Ok(Value::Null),
        _ => temporal_type_error("duration"),
    }
}

fn duration_from_map(
    map: &std::collections::BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<DurationValue> {
    validate_duration_fields(map, control)?;
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

fn validate_duration_fields(
    map: &std::collections::BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<()> {
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
    for name in map.keys() {
        if let Some(control) = control {
            control.work(1)?;
        }
        if !FIELDS.contains(&name.as_str()) {
            return Err(QueryError::semantic(format!(
                "duration() map contains unsupported field {name}"
            )));
        }
    }
    Ok(())
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

fn duration_from_pattern(
    input: &str,
    pattern: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<DurationValue> {
    let (regular_expression, units) = duration_pattern_regex(pattern, control)?;
    if let Some(control) = control {
        control.check()?;
    }
    let (regex, _regex_memory) = super::compile_regex_controlled(&regular_expression, control)?;
    if let Some(control) = control {
        control.check()?;
    }
    let captures = regex.captures(input).ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::Type,
            "duration input does not match its pattern",
        )
    })?;
    if let Some(control) = control {
        control.check()?;
    }
    let mut months = 0.0;
    let mut days = 0.0;
    let mut seconds = 0.0;
    for (index, unit) in units.into_iter().enumerate() {
        if let Some(control) = control {
            control.work(1)?;
        }
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

fn duration_pattern_regex(
    pattern: &str,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(String, Vec<char>)> {
    let mut characters = Vec::with_capacity(pattern.len());
    for character in pattern.chars() {
        if let Some(control) = control {
            control.work(1)?;
        }
        characters.push(character);
    }
    let mut regex = String::from("^");
    let mut units = Vec::new();
    let mut index = 0;
    while index < characters.len() {
        if let Some(control) = control {
            control.work(1)?;
        }
        if characters[index] == '\'' {
            let (literal, next) = duration_pattern_literal(&characters, index, control)?;
            regex.push_str(&duration_escape(&literal, control)?);
            index = next;
            continue;
        }
        let character = characters[index];
        let start = index;
        while index < characters.len() && characters[index] == character {
            if let Some(control) = control {
                control.work(1)?;
            }
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
            if let Some(control) = control {
                control.check()?;
            }
            let repeated = character.to_string().repeat(index - start);
            regex.push_str(&duration_escape(&repeated, control)?);
        }
    }
    regex.push('$');
    Ok((regex, units))
}

fn duration_escape(input: &str, control: Option<&ExecutionControl<'_>>) -> QueryResult<String> {
    if let Some(control) = control {
        control.check()?;
    }
    let escaped = regex::escape(input);
    if let Some(control) = control {
        control.check()?;
    }
    Ok(escaped)
}

fn duration_pattern_literal(
    characters: &[char],
    start: usize,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(String, usize)> {
    let mut literal = String::new();
    let mut index = start + 1;
    while let Some(character) = characters.get(index) {
        if let Some(control) = control {
            control.work(1)?;
        }
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
    let Some(map) = point_map(&values[0])? else {
        return Ok(Value::Null);
    };
    if point_has_null_argument(map) {
        return Ok(Value::Null);
    }
    let (geographic, coordinates) = point_coordinates(map)?;
    let crs = point_crs(map, geographic, coordinates.len() == 3)?;
    validate_point_coordinate_system(geographic, &crs)?;
    PointValue::new(&crs, coordinates)
        .map(Value::Point)
        .map_err(Into::into)
}

fn point_map(value: &Value) -> QueryResult<Option<&std::collections::BTreeMap<String, Value>>> {
    match value {
        Value::Map(map) => Ok(Some(map)),
        Value::Null => Ok(None),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "point() requires Map",
        )),
    }
}

fn point_has_null_argument(map: &std::collections::BTreeMap<String, Value>) -> bool {
    const POINT_ARGUMENTS: &[&str] = &[
        "x",
        "y",
        "z",
        "longitude",
        "latitude",
        "height",
        "crs",
        "srid",
    ];
    POINT_ARGUMENTS
        .iter()
        .any(|name| matches!(map.get(*name), Some(Value::Null)))
}

fn point_coordinates(
    map: &std::collections::BTreeMap<String, Value>,
) -> QueryResult<(bool, Vec<f64>)> {
    let has_cartesian_coordinates = map.contains_key("x") || map.contains_key("y");
    let has_geographic_coordinates = map.contains_key("longitude") || map.contains_key("latitude");
    if has_cartesian_coordinates && has_geographic_coordinates {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "point coordinates must use either x/y or longitude/latitude",
        ));
    }
    if map.contains_key("height") && map.contains_key("z") {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "point coordinates must use either height or z",
        ));
    }
    let geographic = has_geographic_coordinates;
    let first = number(map.get(if geographic { "longitude" } else { "x" }))?;
    let second = number(map.get(if geographic { "latitude" } else { "y" }))?;
    let third = map
        .get("height")
        .or_else(|| map.get("z"))
        .map(number_value)
        .transpose()?;
    let mut coordinates = vec![first, second];
    if let Some(third) = third {
        coordinates.push(third);
    }
    Ok((geographic, coordinates))
}

fn validate_point_coordinate_system(geographic: bool, crs: &str) -> QueryResult<()> {
    if geographic && !crs.starts_with("wgs-84") {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "longitude/latitude coordinates require a WGS-84 coordinate system",
        ));
    }
    Ok(())
}

fn point_crs(
    map: &std::collections::BTreeMap<String, Value>,
    geographic: bool,
    three_dimensional: bool,
) -> QueryResult<String> {
    if map.contains_key("crs") && map.contains_key("srid") {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "point crs and srid cannot both be specified",
        ));
    }
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
    let point_coordinates = point.coordinates();
    let lower_coordinates = lower.coordinates();
    let upper_coordinates = upper.coordinates();
    let longitude_inside = if point.crs().starts_with("wgs-84") {
        let value = point_coordinates[0];
        let lower = lower_coordinates[0];
        let upper = upper_coordinates[0];
        if lower <= upper {
            value >= lower && value <= upper
        } else {
            value >= lower || value <= upper
        }
    } else {
        point_coordinates[0] >= lower_coordinates[0] && point_coordinates[0] <= upper_coordinates[0]
    };
    let remaining_coordinates_inside = point_coordinates[1..]
        .iter()
        .zip(&lower_coordinates[1..])
        .zip(&upper_coordinates[1..])
        .all(|((value, lower), upper)| value >= lower && value <= upper);
    Ok(Value::Boolean(
        longitude_inside && remaining_coordinates_inside,
    ))
}

fn vector(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
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
    let numbers = vector_input(&values[0], coordinate_type, control)?;
    if numbers.len() != dimension {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "vector() input length differs from its dimension",
        ));
    }
    VectorValue::new_with_checkpoint(
        coordinate_type,
        cast_vector(coordinate_type, &numbers, control)?,
        |units| match control {
            Some(control) => control.work(units as u64),
            None => Ok(()),
        },
    )
    .map(Value::Vector)
}

fn vector_similarity(
    values: &[Value],
    cosine: bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    require_arity(values, 2, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    let left = similarity_vector(&values[0], control)?;
    let right = similarity_vector(&values[1], control)?;
    require_same_vector_dimension(&left, &right)?;
    if cosine {
        let similarity = cosine_similarity(&left, &right, control)?;
        Ok(Value::Float(f64::from(
            ((1.0 + similarity) / 2.0).clamp(0.0, 1.0),
        )))
    } else {
        let distance = euclidean_squared_f32(&left, &right, control)?;
        Ok(Value::Float(f64::from(1.0 / (1.0 + distance))))
    }
}

fn vector_distance(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
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
    let left = vector_numbers_f32(left, control)?;
    let right = vector_numbers_f32(right, control)?;
    require_same_vector_dimension(&left, &right)?;
    let distance = match metric.to_ascii_uppercase().as_str() {
        "EUCLIDEAN" => euclidean_squared_f32(&left, &right, control)?.sqrt(),
        "EUCLIDEAN_SQUARED" => euclidean_squared_f32(&left, &right, control)?,
        "MANHATTAN" => sum_pairs(&left, &right, control, |left, right| (left - right).abs())?,
        "COSINE" => 1.0 - cosine_similarity(&left, &right, control)?,
        "DOT" => -sum_pairs(&left, &right, control, |left, right| left * right)?,
        "HAMMING" => sum_pairs(&left, &right, control, |left, right| {
            if left == right { 0.0 } else { 1.0 }
        })?,
        _ => return Err(QueryError::semantic("unsupported vector distance metric")),
    };
    Ok(Value::Float(f64::from(distance)))
}

fn vector_norm(values: &[Value], control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
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
    let values = vector_numbers_f32(value, control)?;
    let result = match metric.to_ascii_uppercase().as_str() {
        "EUCLIDEAN" => sum_pairs(&values, &values, control, |left, right| left * right)?.sqrt(),
        "MANHATTAN" => sum_pairs(&values, &values, control, |left, _| left.abs())?,
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

fn similarity_vector(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Vec<f32>> {
    let values = match value {
        Value::Vector(value) => vector_numbers_f32(value, control)?,
        Value::List(values) => coordinate_vec(
            values
                .iter()
                .map(|value| number_coordinate(value, control).map(|value| value as f32)),
        )?,
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "vector similarity requires Vector or numeric List input",
            ));
        }
    };
    let mut finite = true;
    for value in &values {
        if let Some(control) = control {
            control.work(1)?;
        }
        finite &= value.is_finite();
    }
    if values.is_empty() || !finite {
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

fn cosine_similarity(
    left: &[f32],
    right: &[f32],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<f32> {
    let dot = sum_pairs(left, right, control, |left, right| left * right)?;
    let denominator = sum_pairs(left, left, control, |left, right| left * right)?.sqrt()
        * sum_pairs(right, right, control, |left, right| left * right)?.sqrt();
    if denominator == 0.0 {
        Err(QueryError::new(
            QueryErrorKind::Type,
            "cosine vector input must have a non-zero norm",
        ))
    } else {
        Ok(dot / denominator)
    }
}

fn sum_pairs(
    left: &[f32],
    right: &[f32],
    control: Option<&ExecutionControl<'_>>,
    term: impl Fn(f32, f32) -> f32,
) -> QueryResult<f32> {
    let mut sum = 0.0;
    for (left, right) in left.iter().zip(right) {
        if let Some(control) = control {
            control.work(1)?;
        }
        sum += term(*left, *right);
    }
    Ok(sum)
}

fn euclidean_squared_f32(
    left: &[f32],
    right: &[f32],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<f32> {
    sum_pairs(left, right, control, |left, right| (left - right).powi(2))
}

fn vector_input<'a>(
    value: &'a Value,
    kind: VectorCoordinateType,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<std::borrow::Cow<'a, [Value]>> {
    match value {
        Value::List(values) => Ok(std::borrow::Cow::Borrowed(values)),
        Value::String(value) => {
            let mut output = Vec::with_capacity(vector_string_count(value, control)?);
            let input = super::conversion::trim_controlled(value, control, |character| {
                matches!(character, '[' | ']')
            })?;
            for value in input.split(',') {
                if let Some(control) = control {
                    control.work(1)?;
                    control.check()?;
                }
                output.push(parse_vector_number(value, kind, control)?);
            }
            Ok(std::borrow::Cow::Owned(output))
        }
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "vector() input must be List or String",
        )),
    }
}

fn parse_vector_number(
    value: &str,
    kind: VectorCoordinateType,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Value> {
    let value = super::conversion::trim_controlled(value, control, char::is_whitespace)?;
    if let Ok(value) = value.parse::<i64>() {
        return Ok(Value::Integer(value));
    }
    let digits = value
        .strip_prefix('+')
        .or_else(|| value.strip_prefix('-'))
        .unwrap_or(value);
    let mut integer_digits = !digits.is_empty();
    for digit in digits.bytes() {
        if let Some(control) = control {
            control.bytes(1)?;
        }
        integer_digits &= digit.is_ascii_digit();
    }
    if !matches!(kind, VectorCoordinateType::F32 | VectorCoordinateType::F64) && integer_digits {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "vector integer String coordinate is outside the INTEGER64 range",
        ));
    }
    let number = value
        .parse::<f64>()
        .map_err(|_| QueryError::new(QueryErrorKind::Type, "vector String contains non-number"))?;
    if let Some(control) = control {
        control.check()?;
    }
    Ok(Value::Float(number))
}

fn vector_string_count(value: &str, control: Option<&ExecutionControl<'_>>) -> QueryResult<usize> {
    let mut count = 1_usize;
    for byte in value.bytes() {
        if let Some(control) = control {
            control.bytes(1)?;
        }
        if byte == b',' {
            count = count
                .checked_add(1)
                .ok_or_else(|| super::super::resources::memory_error("vector capacity overflow"))?;
        }
    }
    Ok(count)
}

fn cast_vector(
    kind: VectorCoordinateType,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<VectorValues> {
    Ok(match kind {
        VectorCoordinateType::I8 => VectorValues::I8(coordinate_vec(values.iter().map(|value| {
            cast_integer_coordinate(value, i64::from(i8::MIN), i64::from(i8::MAX), control)
                .map(|value| value as i8)
        }))?),
        VectorCoordinateType::I16 => {
            VectorValues::I16(coordinate_vec(values.iter().map(|value| {
                cast_integer_coordinate(value, i64::from(i16::MIN), i64::from(i16::MAX), control)
                    .map(|value| value as i16)
            }))?)
        }
        VectorCoordinateType::I32 => {
            VectorValues::I32(coordinate_vec(values.iter().map(|value| {
                cast_integer_coordinate(value, i64::from(i32::MIN), i64::from(i32::MAX), control)
                    .map(|value| value as i32)
            }))?)
        }
        VectorCoordinateType::I64 => {
            VectorValues::I64(coordinate_vec(values.iter().map(|value| {
                cast_integer_coordinate(value, i64::MIN, i64::MAX, control)
            }))?)
        }
        VectorCoordinateType::F32 => {
            VectorValues::F32(coordinate_vec(values.iter().map(|value| {
                number_coordinate(value, control).map(|value| value as f32)
            }))?)
        }
        VectorCoordinateType::F64 => VectorValues::F64(coordinate_vec(
            values.iter().map(|value| number_coordinate(value, control)),
        )?),
    })
}

fn coordinate_vec<T>(items: impl ExactSizeIterator<Item = QueryResult<T>>) -> QueryResult<Vec<T>> {
    let mut output = Vec::with_capacity(items.len());
    for value in items {
        output.push(value?);
    }
    Ok(output)
}

fn number_coordinate(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<f64> {
    if let Some(control) = control {
        control.work(1)?;
    }
    number_value(value)
}

fn cast_integer_coordinate(
    value: &Value,
    minimum: i64,
    maximum: i64,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<i64> {
    if let Some(control) = control {
        control.work(1)?;
    }
    let invalid = || {
        QueryError::new(
            QueryErrorKind::Type,
            "vector coordinate cannot be represented by its declared type",
        )
    };
    let value = match value {
        Value::Integer(value) => *value,
        Value::Float(value)
            if value.is_finite()
                && value.fract() == 0.0
                && *value >= i64::MIN as f64
                && *value < -(i64::MIN as f64) =>
        {
            *value as i64
        }
        _ => return Err(invalid()),
    };
    if (minimum..=maximum).contains(&value) {
        Ok(value)
    } else {
        Err(invalid())
    }
}

fn vector_numbers_f32(
    value: &VectorValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<Vec<f32>> {
    macro_rules! convert {
        ($values:expr) => {
            coordinate_vec($values.iter().map(|value| {
                if let Some(control) = control {
                    control.work(1)?;
                }
                Ok((*value as f64) as f32)
            }))
        };
    }
    match value.values() {
        VectorValues::I8(values) => convert!(values),
        VectorValues::I16(values) => convert!(values),
        VectorValues::I32(values) => convert!(values),
        VectorValues::I64(values) => convert!(values),
        VectorValues::F32(values) => convert!(values),
        VectorValues::F64(values) => convert!(values),
    }
}

fn typed_workspace_bytes(
    name: &str,
    values: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    if contains_null(values) && (name.starts_with("vector") || name == "duration") {
        return Ok(0);
    }
    let workspace = match name {
        "vector" => vector_construction_workspace(values, control),
        "vector.similarity.cosine" | "vector.similarity.euclidean" | "vector_distance" => {
            vector_numeric_workspace(values, 2)
        }
        "vector_norm" => vector_numeric_workspace(values, 1),
        "duration" => duration_workspace(values),
        "point" => point_workspace(values),
        "randomuuid" => Ok(36),
        "uuid" if matches!(values, [Value::String(_)]) => Ok(64),
        _ => Ok(0),
    }?;
    checked_add(workspace, vector_type_name_workspace(name, values))
}

fn vector_type_name_workspace(name: &str, values: &[Value]) -> u64 {
    let index = match name {
        "vector" | "vector_distance" => 2,
        "vector_norm" => 1,
        _ => return 0,
    };
    match values.get(index) {
        Some(Value::String(value)) => value.len() as u64,
        _ => 0,
    }
}

fn vector_construction_workspace(
    values: &[Value],
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let mut bytes = 0;
    let count = match values.first() {
        Some(Value::List(values)) => values.len(),
        Some(Value::String(value)) => {
            let count = vector_string_count(value, Some(control))?;
            bytes = allocation_bytes::<Value>(count)?;
            count
        }
        _ => 0,
    };
    checked_add(bytes, allocation_bytes::<f64>(count)?)
}

fn vector_numeric_workspace(values: &[Value], inputs: usize) -> QueryResult<u64> {
    values.iter().take(inputs).try_fold(0, |bytes, value| {
        let count = match value {
            Value::Vector(value) => value.dimension(),
            Value::List(values) => values.len(),
            _ => 0,
        };
        checked_add(bytes, allocation_bytes::<f32>(count)?)
    })
}

fn duration_workspace(values: &[Value]) -> QueryResult<u64> {
    // Pattern construction owns chars, capture units, regex source and escaped
    // literals at once. Include geometric container growth conservatively; the
    // borrowed input text itself is not copied during regex matching.
    if let [Value::String(_), Value::String(pattern)] = values {
        return checked_add(256, checked_mul(pattern.len() as u64, 128)?);
    }
    Ok(256)
}

fn point_workspace(values: &[Value]) -> QueryResult<u64> {
    let Some(Value::Map(values)) = values.first() else {
        return Ok(0);
    };
    if point_has_null_argument(values) {
        return Ok(0);
    }
    match values.get("crs") {
        Some(Value::String(value)) => checked_add(128, checked_mul(value.len() as u64, 2)?),
        _ => Ok(128),
    }
}

fn geographic_distance(left: &[f64], right: &[f64]) -> f64 {
    const WGS84_EARTH_RADIUS_METERS: f64 = 6_378_140.0;
    let longitude = (right[0] - left[0]).to_radians();
    let latitude = (right[1] - left[1]).to_radians();
    let a = (latitude / 2.0).sin().powi(2)
        + left[1].to_radians().cos()
            * right[1].to_radians().cos()
            * (longitude / 2.0).sin().powi(2);
    let a = a.clamp(0.0, 1.0);
    let central_angle = 2.0 * a.sqrt().atan2((1.0 - a).sqrt());
    let average_height = if left.len() == 3 && right.len() == 3 {
        (left[2] + right[2]) / 2.0
    } else {
        0.0
    };
    let surface = (WGS84_EARTH_RADIUS_METERS + average_height) * central_angle;
    if left.len() == 3 && right.len() == 3 {
        surface.hypot(right[2] - left[2])
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::resources::QueryResources;
    use std::cell::Cell;

    #[test]
    fn duration_cancels_inside_numeric_text_scan() {
        let checks = Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        };
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &interrupted);
        let text = format!("P{}D", "0".repeat(256 * 1024));
        let error = evaluate_controlled("duration", &[Value::String(text)], Some(&control))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(checks.get(), 3);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn duration_extended_syntax_and_component_errors_stay_compatible() {
        let resources = QueryResources::new(Some(4096));
        let control = ExecutionControl::new(&resources, &|| false);
        for text in ["P0001-02-03T04:05:06", "P1Y2M3DT4H5M6S", "P000000000D"] {
            let output = evaluate_controlled(
                "duration",
                &[Value::String(text.to_owned())],
                Some(&control),
            )
            .unwrap()
            .unwrap();
            assert_eq!(output, Value::Duration(DurationValue::parse(text).unwrap()));
        }
        let error = evaluate_controlled(
            "duration",
            &[Value::String("P1D2Y".to_owned())],
            Some(&control),
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Type);
    }

    #[test]
    fn returned_vector_keeps_output_lease() {
        let resources = QueryResources::new(Some(4096));
        let control = ExecutionControl::new(&resources, &|| false);
        let input = [
            Value::List(vec![Value::Integer(1); 32]),
            Value::Integer(32),
            Value::String("FLOAT64".to_owned()),
        ];
        let mut memory = None;
        let output = evaluate_with_memory("vector", &input, Some(&control), &mut memory)
            .unwrap()
            .unwrap();
        assert!(resources.used() >= 32 * 8);
        drop(output);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn string_vector_accounts_for_intermediate_values() {
        let resources = QueryResources::new(Some(16 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let input = [
            Value::String(vec!["1"; 2048].join(",")),
            Value::Integer(2048),
            Value::String("FLOAT64".to_owned()),
        ];
        let error = evaluate_controlled("vector", &input, Some(&control))
            .unwrap()
            .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.peak(), 0);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn vector_work_responds_during_conversion() {
        let checks = Cell::new(0);
        let interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        };
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &interrupted);
        let vector = Value::Vector(
            VectorValue::new(
                VectorCoordinateType::F32,
                VectorValues::F32(vec![1.0; 4096]),
            )
            .unwrap(),
        );
        let error = evaluate_controlled(
            "vector_norm",
            &[vector, Value::String("EUCLIDEAN".to_owned())],
            Some(&control),
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(checks.get(), 3);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn vector_construction_cancels_within_256_finite_checks() {
        const DIMENSION: usize = 4096;
        const CONVERSION_CHECKS: usize = DIMENSION / 256;
        for coordinate_type in ["FLOAT32", "FLOAT64"] {
            let checks = Cell::new(0);
            let interrupted = || {
                checks.set(checks.get() + 1);
                checks.get() > CONVERSION_CHECKS
            };
            let resources = QueryResources::default();
            let control = ExecutionControl::new(&resources, &interrupted);
            let input = [
                Value::List(vec![Value::Float(1.0); DIMENSION]),
                Value::Integer(DIMENSION as i64),
                Value::String(coordinate_type.to_owned()),
            ];
            let error = vector(&input, Some(&control)).unwrap_err();
            assert_eq!(error.kind, QueryErrorKind::Interrupted);
            assert_eq!(checks.get(), CONVERSION_CHECKS + 1);
        }
    }
}
