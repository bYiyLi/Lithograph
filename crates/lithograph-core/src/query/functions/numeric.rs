use crate::cypher::Value;

use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::contains_null;
use super::require_exact_arity as require_arity;

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let result = match name {
        "e" => zero(values, std::f64::consts::E),
        "pi" => zero(values, std::f64::consts::PI),
        "rand" => random(values),
        "abs" => unary_numeric(values, abs),
        "acos" => unary_float(values, f64::acos),
        "asin" => unary_float(values, f64::asin),
        "atan" => unary_float(values, f64::atan),
        "ceil" | "ceiling" => unary_float(values, f64::ceil),
        "cos" => unary_float(values, f64::cos),
        "cosh" => unary_float(values, f64::cosh),
        "cot" => unary_float(values, |value| 1.0 / value.tan()),
        "coth" => unary_float(values, |value| 1.0 / value.tanh()),
        "degrees" => unary_float(values, f64::to_degrees),
        "exp" => unary_float(values, f64::exp),
        "floor" => unary_float(values, f64::floor),
        "haversin" => unary_float(values, |value| (1.0 - value.cos()) / 2.0),
        "isnan" => unary_float_bool(values, f64::is_nan),
        "ln" | "log" => unary_float(values, f64::ln),
        "log10" => unary_float(values, f64::log10),
        "radians" => unary_float(values, f64::to_radians),
        "sign" => unary_numeric(values, sign),
        "sin" => unary_float(values, f64::sin),
        "sinh" => unary_float(values, f64::sinh),
        "sqrt" => unary_float(values, f64::sqrt),
        "tan" => unary_float(values, f64::tan),
        "tanh" => unary_float(values, f64::tanh),
        "atan2" => binary_float(values, f64::atan2),
        "round" => round(values),
        _ => return None,
    };
    Some(result)
}

fn zero(values: &[Value], result: f64) -> QueryResult<Value> {
    require_arity(values, 0)?;
    Ok(Value::Float(result))
}

fn random(values: &[Value]) -> QueryResult<Value> {
    require_arity(values, 0)?;
    let mut bytes = [0_u8; 8];
    getrandom::fill(&mut bytes)
        .map_err(|error| QueryError::internal(format!("random source failed: {error}")))?;
    let bits = u64::from_le_bytes(bytes) >> 11;
    Ok(Value::Float(bits as f64 / (1_u64 << 53) as f64))
}

fn unary_numeric(
    values: &[Value],
    operation: impl FnOnce(Value) -> QueryResult<Value>,
) -> QueryResult<Value> {
    require_arity(values, 1)?;
    if matches!(values[0], Value::Null) {
        return Ok(Value::Null);
    }
    operation(values[0].clone())
}

fn abs(value: Value) -> QueryResult<Value> {
    match value {
        Value::Integer(value) => value
            .checked_abs()
            .map(Value::Integer)
            .ok_or_else(|| QueryError::new(QueryErrorKind::Type, "INTEGER64 abs overflow")),
        Value::Float(value) => Ok(Value::Float(value.abs())),
        _ => numeric_type_error(),
    }
}

fn sign(value: Value) -> QueryResult<Value> {
    match value {
        Value::Integer(value) => Ok(Value::Integer(value.signum())),
        Value::Float(value) => Ok(Value::Integer(if value > 0.0 {
            1
        } else if value < 0.0 {
            -1
        } else {
            0
        })),
        _ => numeric_type_error(),
    }
}

fn unary_float(values: &[Value], operation: impl FnOnce(f64) -> f64) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match values[0] {
        Value::Null => Ok(Value::Null),
        Value::Integer(value) => Ok(Value::Float(operation(value as f64))),
        Value::Float(value) => Ok(Value::Float(operation(value))),
        _ => numeric_type_error(),
    }
}

fn unary_float_bool(values: &[Value], operation: impl FnOnce(f64) -> bool) -> QueryResult<Value> {
    require_arity(values, 1)?;
    match values[0] {
        Value::Null => Ok(Value::Null),
        Value::Integer(value) => Ok(Value::Boolean(operation(value as f64))),
        Value::Float(value) => Ok(Value::Boolean(operation(value))),
        _ => numeric_type_error(),
    }
}

fn binary_float(values: &[Value], operation: impl FnOnce(f64, f64) -> f64) -> QueryResult<Value> {
    require_arity(values, 2)?;
    if contains_null(values) {
        return Ok(Value::Null);
    }
    Ok(Value::Float(operation(
        number(&values[0])?,
        number(&values[1])?,
    )))
}

fn round(values: &[Value]) -> QueryResult<Value> {
    if !(1..=3).contains(&values.len()) {
        return Err(QueryError::semantic(
            "round() expects value, optional precision, and optional mode",
        ));
    }
    if matches!(values[0], Value::Null) {
        return Ok(Value::Null);
    }
    let value = number(&values[0])?;
    let precision = match values.get(1) {
        None => 0_i32,
        Some(Value::Integer(value)) => i32::try_from(*value).map_err(|_| {
            QueryError::new(
                QueryErrorKind::Type,
                "round() precision is outside INTEGER32",
            )
        })?,
        Some(Value::Float(value))
            if value.is_finite()
                && value.fract() == 0.0
                && *value >= f64::from(i32::MIN)
                && *value <= f64::from(i32::MAX) =>
        {
            *value as i32
        }
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "round() precision must be an integer-valued Integer or Float",
            ));
        }
    };
    let scale = 10_f64.powi(precision);
    let scaled = value * scale;
    let rounded = match values.get(2) {
        None if values.len() == 1 || precision == 0 => round_ties_positive(scaled),
        None => round_half(scaled, HalfMode::AwayFromZero),
        Some(Value::String(mode)) => match mode.to_ascii_uppercase().as_str() {
            "UP" => round_away_from_zero(scaled),
            "DOWN" => scaled.trunc(),
            "CEILING" => scaled.ceil(),
            "FLOOR" => scaled.floor(),
            "HALF_UP" => round_half(scaled, HalfMode::AwayFromZero),
            "HALF_DOWN" => round_half(scaled, HalfMode::TowardZero),
            "HALF_EVEN" => round_half(scaled, HalfMode::Even),
            _ => {
                return Err(QueryError::new(
                    QueryErrorKind::Type,
                    "round() mode must be UP, DOWN, CEILING, FLOOR, HALF_UP, HALF_DOWN, or HALF_EVEN",
                ));
            }
        },
        Some(Value::Null) => return Ok(Value::Null),
        Some(_) => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "round() mode must be String",
            ));
        }
    };
    Ok(Value::Float(rounded / scale))
}

#[derive(Clone, Copy)]
enum HalfMode {
    AwayFromZero,
    TowardZero,
    Even,
}

fn round_ties_positive(value: f64) -> f64 {
    if value.is_finite() {
        (value + 0.5).floor()
    } else {
        value
    }
}

fn round_away_from_zero(value: f64) -> f64 {
    if value == value.trunc() {
        value
    } else if value.is_sign_negative() {
        value.floor()
    } else {
        value.ceil()
    }
}

fn round_half(value: f64, mode: HalfMode) -> f64 {
    if !value.is_finite() {
        return value;
    }
    let lower = value.floor();
    let fraction = value - lower;
    let tolerance = f64::EPSILON * value.abs().max(1.0) * 4.0;
    if (fraction - 0.5).abs() > tolerance {
        return if fraction < 0.5 { lower } else { lower + 1.0 };
    }
    match mode {
        HalfMode::AwayFromZero if value.is_sign_negative() => lower,
        HalfMode::AwayFromZero => lower + 1.0,
        HalfMode::TowardZero if value.is_sign_negative() => lower + 1.0,
        HalfMode::TowardZero => lower,
        HalfMode::Even if lower.rem_euclid(2.0) == 0.0 => lower,
        HalfMode::Even => lower + 1.0,
    }
}

fn number(value: &Value) -> QueryResult<f64> {
    match value {
        Value::Integer(value) => Ok(*value as f64),
        Value::Float(value) => Ok(*value),
        _ => numeric_type_error(),
    }
}

fn numeric_type_error<T>() -> QueryResult<T> {
    Err(QueryError::new(
        QueryErrorKind::Type,
        "numeric function requires Integer or Float",
    ))
}
