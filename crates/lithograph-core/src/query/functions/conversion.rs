use crate::cypher::{Value, VectorValues};

use super::super::expression::value_to_string;
use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::contains_null;

pub(super) fn evaluate(name: &str, values: &[Value]) -> Option<QueryResult<Value>> {
    let result = match name {
        "toboolean" => convert_one(values, to_boolean, false),
        "tobooleanornull" => convert_one(values, to_boolean, true),
        "tointeger" => convert_one(values, to_integer, false),
        "tointegerornull" => convert_one(values, to_integer, true),
        "tofloat" => convert_one(values, to_float, false),
        "tofloatornull" => convert_one(values, to_float, true),
        "tostring" => convert_one(values, to_string, false),
        "tostringornull" => convert_one(values, to_string, true),
        "tobooleanlist" => convert_list(values, to_boolean),
        "tointegerlist" => convert_list_or_vector(values, to_integer),
        "tofloatlist" => convert_list_or_vector(values, to_float),
        "tostringlist" => convert_list(values, to_string),
        "valuetype" => value_type(values),
        _ => return None,
    };
    Some(result)
}

fn convert_list_or_vector(
    values: &[Value],
    conversion: fn(&Value) -> QueryResult<Value>,
) -> QueryResult<Value> {
    require_one(values)?;
    if let Value::Vector(vector) = &values[0] {
        let values: Vec<Value> = match vector.values() {
            VectorValues::I8(values) => values
                .iter()
                .map(|value| Value::Integer(i64::from(*value)))
                .collect(),
            VectorValues::I16(values) => values
                .iter()
                .map(|value| Value::Integer(i64::from(*value)))
                .collect(),
            VectorValues::I32(values) => values
                .iter()
                .map(|value| Value::Integer(i64::from(*value)))
                .collect(),
            VectorValues::I64(values) => values.iter().copied().map(Value::Integer).collect(),
            VectorValues::F32(values) => values
                .iter()
                .map(|value| Value::Float(f64::from(*value)))
                .collect(),
            VectorValues::F64(values) => values.iter().copied().map(Value::Float).collect(),
        };
        return Ok(Value::List(
            values
                .iter()
                .map(|value| conversion(value).unwrap_or(Value::Null))
                .collect(),
        ));
    }
    convert_list(values, conversion)
}

fn convert_one(
    values: &[Value],
    conversion: fn(&Value) -> QueryResult<Value>,
    or_null: bool,
) -> QueryResult<Value> {
    require_one(values)?;
    if matches!(values[0], Value::Null) {
        return Ok(Value::Null);
    }
    match conversion(&values[0]) {
        Ok(value) => Ok(value),
        Err(_) if or_null => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

fn convert_list(
    values: &[Value],
    conversion: fn(&Value) -> QueryResult<Value>,
) -> QueryResult<Value> {
    require_one(values)?;
    let Value::List(values) = &values[0] else {
        if matches!(values[0], Value::Null) {
            return Ok(Value::Null);
        }
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "list conversion requires a List or null",
        ));
    };
    Ok(Value::List(
        values
            .iter()
            .map(|value| {
                if matches!(value, Value::Null) {
                    Value::Null
                } else {
                    conversion(value).unwrap_or(Value::Null)
                }
            })
            .collect(),
    ))
}

fn to_boolean(value: &Value) -> QueryResult<Value> {
    match value {
        Value::Boolean(value) => Ok(Value::Boolean(*value)),
        Value::Integer(value) => Ok(Value::Boolean(*value != 0)),
        Value::String(value) if value.eq_ignore_ascii_case("true") => Ok(Value::Boolean(true)),
        Value::String(value) if value.eq_ignore_ascii_case("false") => Ok(Value::Boolean(false)),
        Value::String(_) => Ok(Value::Null),
        _ => conversion_error("Boolean"),
    }
}

fn to_integer(value: &Value) -> QueryResult<Value> {
    match value {
        Value::Integer(value) => Ok(Value::Integer(*value)),
        Value::Float(value)
            if value.is_finite()
                && *value >= i64::MIN as f64
                && *value < 9_223_372_036_854_775_808.0 =>
        {
            Ok(Value::Integer(value.trunc() as i64))
        }
        Value::Float(_) => Ok(Value::Null),
        Value::Boolean(value) => Ok(Value::Integer(i64::from(*value))),
        Value::String(value) => {
            let value = value.trim();
            if let Ok(value) = value.parse::<i64>() {
                return Ok(Value::Integer(value));
            }
            match value.parse::<f64>() {
                Ok(value)
                    if value.is_finite()
                        && value >= i64::MIN as f64
                        && value < 9_223_372_036_854_775_808.0 =>
                {
                    Ok(Value::Integer(value.trunc() as i64))
                }
                _ => Ok(Value::Null),
            }
        }
        _ => conversion_error("Integer"),
    }
}

fn to_float(value: &Value) -> QueryResult<Value> {
    match value {
        Value::Float(value) => Ok(Value::Float(*value)),
        Value::Integer(value) => Ok(Value::Float(*value as f64)),
        Value::String(value) => value
            .trim()
            .parse::<f64>()
            .map(Value::Float)
            .or(Ok(Value::Null)),
        _ => conversion_error("Float"),
    }
}

fn to_string(value: &Value) -> QueryResult<Value> {
    value_to_string(value).map(Value::String)
}

fn value_type(values: &[Value]) -> QueryResult<Value> {
    require_one(values)?;
    let name = match &values[0] {
        Value::Null => "NULL".to_owned(),
        value => runtime_type(value).render(false),
    };
    Ok(Value::String(name))
}

#[derive(Clone, Eq, PartialEq)]
struct RuntimeType {
    name: String,
    rank: u16,
}

impl RuntimeType {
    fn simple(name: &str, rank: u16) -> Self {
        Self {
            name: name.to_owned(),
            rank,
        }
    }

    fn render(&self, nullable: bool) -> String {
        if nullable {
            self.name.clone()
        } else {
            format!("{} NOT NULL", self.name)
        }
    }
}

fn runtime_type(value: &Value) -> RuntimeType {
    match value {
        Value::Null => RuntimeType::simple("NULL", 1),
        Value::Boolean(_) => RuntimeType::simple("BOOLEAN", 2),
        Value::String(_) => RuntimeType::simple("STRING", 3),
        Value::Integer(_) => RuntimeType::simple("INTEGER", 7),
        Value::Float(_) => RuntimeType::simple("FLOAT", 9),
        Value::Date(_) => RuntimeType::simple("DATE", 10),
        Value::LocalTime(_) => RuntimeType::simple("LOCAL TIME", 11),
        Value::Time(_) => RuntimeType::simple("ZONED TIME", 12),
        Value::LocalDateTime(_) => RuntimeType::simple("LOCAL DATETIME", 13),
        Value::ZonedDateTime(_) => RuntimeType::simple("ZONED DATETIME", 14),
        Value::Duration(_) => RuntimeType::simple("DURATION", 15),
        Value::Point(_) => RuntimeType::simple("POINT", 16),
        Value::Node(_) => RuntimeType::simple("NODE", 17),
        Value::Relationship(_) => RuntimeType::simple("RELATIONSHIP", 18),
        Value::Uuid(_) => RuntimeType::simple("UUID", 19),
        Value::Vector(value) => RuntimeType {
            name: format!(
                "VECTOR<{}>({})",
                vector_coordinate_type(value.coordinate_type()),
                value.dimension()
            ),
            rank: 20,
        },
        Value::Map(_) => RuntimeType::simple("MAP", 100),
        Value::List(values) => RuntimeType {
            name: format!("LIST<{}>", list_inner_type(values)),
            rank: 101,
        },
        Value::Path(_) => RuntimeType::simple("PATH", 102),
    }
}

fn vector_coordinate_type(value: crate::cypher::VectorCoordinateType) -> &'static str {
    use crate::cypher::VectorCoordinateType;
    match value {
        VectorCoordinateType::I8 => "INTEGER8 NOT NULL",
        VectorCoordinateType::I16 => "INTEGER16 NOT NULL",
        VectorCoordinateType::I32 => "INTEGER32 NOT NULL",
        VectorCoordinateType::I64 => "INTEGER NOT NULL",
        VectorCoordinateType::F32 => "FLOAT32 NOT NULL",
        VectorCoordinateType::F64 => "FLOAT NOT NULL",
    }
}

fn list_inner_type(values: &[Value]) -> String {
    if values.is_empty() {
        return "NOTHING".to_owned();
    }
    let nullable = contains_null(values);
    let mut types = values
        .iter()
        .filter(|value| !matches!(value, Value::Null))
        .map(runtime_type)
        .collect::<Vec<_>>();
    types.sort_by(|left, right| left.rank.cmp(&right.rank).then(left.name.cmp(&right.name)));
    types.dedup_by(|left, right| left.name == right.name);
    if types.is_empty() {
        return "NULL".to_owned();
    }
    types
        .iter()
        .map(|value| value.render(nullable))
        .collect::<Vec<_>>()
        .join(" | ")
}

fn require_one(values: &[Value]) -> QueryResult<()> {
    if values.len() == 1 {
        Ok(())
    } else {
        Err(QueryError::semantic(
            "conversion function expects one argument",
        ))
    }
}

fn conversion_error<T>(target: &str) -> QueryResult<T> {
    Err(conversion_error_value(target))
}

fn conversion_error_value(target: &str) -> QueryError {
    QueryError::new(
        QueryErrorKind::Type,
        format!("value cannot be converted to {target}"),
    )
}
