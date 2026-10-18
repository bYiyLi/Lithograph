use crate::cypher::{Value, VectorValues};

use super::super::expression::{
    clone_string_controlled, value_bytes_with_control, value_to_string,
};
use super::super::resources::{
    ExecutionControl, MemoryReservation, allocation_bytes, checked_add, map_entry_bytes,
};
use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::{append_text_controlled, build_value};

super::legacy_evaluators!();

pub(super) fn evaluate_with_memory(
    name: &str,
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> Option<QueryResult<Value>> {
    let result = match name {
        "toboolean" => convert_one(values, Conversion::Boolean, false, control, result_memory),
        "tobooleanornull" => convert_one(values, Conversion::Boolean, true, control, result_memory),
        "tointeger" => convert_one(values, Conversion::Integer, false, control, result_memory),
        "tointegerornull" => convert_one(values, Conversion::Integer, true, control, result_memory),
        "tofloat" => convert_one(values, Conversion::Float, false, control, result_memory),
        "tofloatornull" => convert_one(values, Conversion::Float, true, control, result_memory),
        "tostring" => convert_one(values, Conversion::String, false, control, result_memory),
        "tostringornull" => convert_one(values, Conversion::String, true, control, result_memory),
        "tobooleanlist" => convert_list(values, Conversion::Boolean, false, control, result_memory),
        "tointegerlist" => convert_list(values, Conversion::Integer, true, control, result_memory),
        "tofloatlist" => convert_list(values, Conversion::Float, true, control, result_memory),
        "tostringlist" => convert_list(values, Conversion::String, false, control, result_memory),
        "valuetype" => value_type(values, control, result_memory),
        _ => return None,
    };
    Some(result)
}

#[derive(Clone, Copy)]
enum Conversion {
    Boolean,
    Integer,
    Float,
    String,
}

impl Conversion {
    fn apply(
        self,
        value: &Value,
        control: Option<&ExecutionControl<'_>>,
        result_memory: &mut Option<MemoryReservation>,
    ) -> QueryResult<Value> {
        if let Some(control) = control {
            control.work(1)?;
            if matches!(value, Value::String(_)) {
                control.check()?;
            }
        }
        let result = match self {
            Self::Boolean => to_boolean(value),
            Self::Integer => to_integer(value, control),
            Self::Float => to_float(value, control),
            Self::String => convert_string(value, control, result_memory),
        };
        if let Some(control) = control
            && matches!(value, Value::String(_))
        {
            control.check()?;
        }
        result
    }
}

fn convert_string(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    if let Value::String(value) = value {
        return build_value(value.len() as u64, control, result_memory, || {
            clone_string_controlled(value, control).map(Value::String)
        });
    }
    if matches!(
        value,
        Value::List(_)
            | Value::Map(_)
            | Value::Node(_)
            | Value::Relationship(_)
            | Value::Path(_)
            | Value::Vector(_)
    ) {
        return to_string(value);
    }
    let bytes = checked_add(value_bytes_with_control(value, control)?, 256)?;
    let mut memory = control.map(|control| control.reserve(bytes)).transpose()?;
    let result = to_string(value)?;
    if let (Some(memory), Value::String(text)) = (&mut memory, &result) {
        memory.resize(text.capacity() as u64)?;
    }
    *result_memory = memory;
    Ok(result)
}

fn conversion_input(values: &[Value]) -> QueryResult<Option<&Value>> {
    require_one(values)?;
    Ok(if matches!(values[0], Value::Null) {
        None
    } else {
        Some(&values[0])
    })
}

fn convert_one(
    values: &[Value],
    conversion: Conversion,
    or_null: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let Some(input) = conversion_input(values)? else {
        return Ok(Value::Null);
    };
    match conversion.apply(input, control, result_memory) {
        Ok(value) => Ok(value),
        Err(error) if or_null && error.kind == QueryErrorKind::Type => Ok(Value::Null),
        Err(error) => Err(error),
    }
}

fn convert_list(
    values: &[Value],
    conversion: Conversion,
    allow_vector: bool,
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    let Some(input) = conversion_input(values)? else {
        return Ok(Value::Null);
    };
    let count = match input {
        Value::List(values) => values.len(),
        Value::Vector(vector) if allow_vector => vector.dimension(),
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "list conversion requires a List or null",
            ));
        }
    };
    let mut memory = control
        .map(|control| control.reserve(allocation_bytes::<Value>(count)?))
        .transpose()?;
    let mut output = Vec::with_capacity(count);
    match input {
        Value::List(values) => {
            for value in values {
                if let Some(control) = control {
                    control.work(1)?;
                }
                append_conversion(value, conversion, control, &mut memory, &mut output)?;
            }
        }
        Value::Vector(vector) => convert_vector_values(vector.values(), |value| {
            append_conversion(&value, conversion, control, &mut memory, &mut output)
        })?,
        _ => unreachable!(),
    }
    *result_memory = memory;
    Ok(Value::List(output))
}

fn append_conversion(
    value: &Value,
    conversion: Conversion,
    control: Option<&ExecutionControl<'_>>,
    memory: &mut Option<MemoryReservation>,
    output: &mut Vec<Value>,
) -> QueryResult<()> {
    let mut item_memory = None;
    let value = if matches!(value, Value::Null) {
        Value::Null
    } else {
        match conversion.apply(value, control, &mut item_memory) {
            Ok(value) => value,
            Err(error) if error.kind == QueryErrorKind::Type => Value::Null,
            Err(error) => return Err(error),
        }
    };
    if let (Some(memory), Some(item_memory)) = (memory, item_memory) {
        memory.merge(item_memory)?;
    }
    output.push(value);
    Ok(())
}

fn convert_vector_values(
    values: &VectorValues,
    mut append: impl FnMut(Value) -> QueryResult<()>,
) -> QueryResult<()> {
    match values {
        VectorValues::I8(values) => {
            for value in values {
                append(Value::Integer(i64::from(*value)))?;
            }
        }
        VectorValues::I16(values) => {
            for value in values {
                append(Value::Integer(i64::from(*value)))?;
            }
        }
        VectorValues::I32(values) => {
            for value in values {
                append(Value::Integer(i64::from(*value)))?;
            }
        }
        VectorValues::I64(values) => {
            for value in values {
                append(Value::Integer(*value))?;
            }
        }
        VectorValues::F32(values) => {
            for value in values {
                append(Value::Float(f64::from(*value)))?;
            }
        }
        VectorValues::F64(values) => {
            for value in values {
                append(Value::Float(*value))?;
            }
        }
    }
    Ok(())
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

fn to_integer(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
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
            let value = trim_controlled(value, control, char::is_whitespace)?;
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

fn to_float(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<Value> {
    match value {
        Value::Float(value) => Ok(Value::Float(*value)),
        Value::Integer(value) => Ok(Value::Float(*value as f64)),
        Value::String(value) => trim_controlled(value, control, char::is_whitespace)?
            .parse::<f64>()
            .map(Value::Float)
            .or(Ok(Value::Null)),
        _ => conversion_error("Float"),
    }
}

pub(super) fn trim_controlled<'a>(
    input: &'a str,
    control: Option<&ExecutionControl<'_>>,
    predicate: fn(char) -> bool,
) -> QueryResult<&'a str> {
    let mut start = 0;
    for (offset, character) in input.char_indices() {
        if let Some(control) = control {
            control.bytes(character.len_utf8() as u64)?;
        }
        if !predicate(character) {
            break;
        }
        start = offset + character.len_utf8();
    }
    let mut end = input.len();
    for (offset, character) in input[start..].char_indices().rev() {
        if let Some(control) = control {
            control.bytes(character.len_utf8() as u64)?;
        }
        if !predicate(character) {
            break;
        }
        end = start + offset;
    }
    Ok(&input[start..end])
}

fn to_string(value: &Value) -> QueryResult<Value> {
    value_to_string(value).map(Value::String)
}

fn value_type(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
    result_memory: &mut Option<MemoryReservation>,
) -> QueryResult<Value> {
    require_one(values)?;
    let kind = runtime_type(&values[0], control)?;
    let (name, memory) = if matches!(values[0], Value::Null) {
        (kind.name, kind.memory)
    } else {
        kind.render(control)?
    };
    *result_memory = memory;
    Ok(Value::String(name))
}

struct RuntimeType {
    name: String,
    rank: u16,
    memory: Option<MemoryReservation>,
}

impl RuntimeType {
    fn simple(name: &str, rank: u16, control: Option<&ExecutionControl<'_>>) -> QueryResult<Self> {
        let memory = control
            .map(|control| control.reserve(name.len() as u64))
            .transpose()?;
        Ok(Self {
            name: name.to_owned(),
            rank,
            memory,
        })
    }

    fn render(
        mut self,
        control: Option<&ExecutionControl<'_>>,
    ) -> QueryResult<(String, Option<MemoryReservation>)> {
        let bytes = checked_add(self.name.len() as u64, 9)?.max(self.name.capacity() as u64);
        if let Some(memory) = &mut self.memory {
            memory.resize(bytes)?;
        }
        if let Some(control) = control {
            control.check()?;
        }
        self.name.reserve_exact(9);
        self.name.push_str(" NOT NULL");
        Ok((self.name, self.memory))
    }
}

fn runtime_type(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<RuntimeType> {
    if let Some(control) = control {
        control.work(1)?;
    }
    let (name, rank) = match value {
        Value::Null => ("NULL", 1),
        Value::Boolean(_) => ("BOOLEAN", 2),
        Value::String(_) => ("STRING", 3),
        Value::Integer(_) => ("INTEGER", 7),
        Value::Float(_) => ("FLOAT", 9),
        Value::Date(_) => ("DATE", 10),
        Value::LocalTime(_) => ("LOCAL TIME", 11),
        Value::Time(_) => ("ZONED TIME", 12),
        Value::LocalDateTime(_) => ("LOCAL DATETIME", 13),
        Value::ZonedDateTime(_) => ("ZONED DATETIME", 14),
        Value::Duration(_) => ("DURATION", 15),
        Value::Point(_) => ("POINT", 16),
        Value::Node(_) => ("NODE", 17),
        Value::Relationship(_) => ("RELATIONSHIP", 18),
        Value::Uuid(_) => ("UUID", 19),
        Value::Vector(value) => {
            let memory = control.map(|control| control.reserve(128)).transpose()?;
            return Ok(RuntimeType {
                name: format!(
                    "VECTOR<{}>({})",
                    vector_coordinate_type(value.coordinate_type()),
                    value.dimension()
                ),
                rank: 20,
                memory,
            });
        }
        Value::Map(_) => ("MAP", 100),
        Value::List(values) => {
            let (inner, _inner_memory) = list_inner_type(values, control)?;
            let bytes = checked_add(inner.len() as u64, 6)?;
            let memory = control.map(|control| control.reserve(bytes)).transpose()?;
            let mut name = String::with_capacity(bytes as usize);
            name.push_str("LIST<");
            append_text_controlled(&mut name, &inner, control)?;
            name.push('>');
            return Ok(RuntimeType {
                name,
                rank: 101,
                memory,
            });
        }
        Value::Path(_) => ("PATH", 102),
    };
    RuntimeType::simple(name, rank, control)
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

fn list_inner_type(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(String, Option<MemoryReservation>)> {
    if values.is_empty() {
        let memory = control.map(|control| control.reserve(7)).transpose()?;
        return Ok(("NOTHING".to_owned(), memory));
    }
    let (types, _set_memory, nullable) = collect_runtime_types(values, control)?;
    if types.is_empty() {
        let memory = control.map(|control| control.reserve(4)).transpose()?;
        return Ok(("NULL".to_owned(), memory));
    }
    render_runtime_types(types, nullable, control)
}

fn render_runtime_types(
    types: RuntimeTypes,
    nullable: bool,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(String, Option<MemoryReservation>)> {
    let bytes = types.iter().try_fold(0, |bytes, (_, name)| {
        if let Some(control) = control {
            control.work(1)?;
        }
        checked_add(
            bytes,
            checked_add(name.len() as u64, if nullable { 3 } else { 12 })?,
        )
    })?;
    let memory = control.map(|control| control.reserve(bytes)).transpose()?;
    let mut output = String::with_capacity(bytes as usize);
    for (_, name) in types {
        if let Some(control) = control {
            control.work(1)?;
        }
        if !output.is_empty() {
            output.push_str(" | ");
        }
        append_text_controlled(&mut output, &name, control)?;
        if !nullable {
            output.push_str(" NOT NULL");
        }
    }
    Ok((output, memory))
}

type RuntimeTypes = std::collections::BTreeSet<(u16, String)>;

fn collect_runtime_types(
    values: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<(RuntimeTypes, Option<MemoryReservation>, bool)> {
    let mut nullable = false;
    let mut types = std::collections::BTreeSet::new();
    let mut set_memory = control.map(|control| control.reserve(0)).transpose()?;
    for value in values {
        if let Some(control) = control {
            control.work(1)?;
        }
        if matches!(value, Value::Null) {
            nullable = true;
        } else {
            let kind = runtime_type(value, control)?;
            let key = (kind.rank, kind.name);
            if !contains_runtime_type(&types, &key, control)? {
                if let Some(memory) = &mut set_memory {
                    memory.grow(map_entry_bytes::<(u16, String)>()?)?;
                    if let Some(kind_memory) = kind.memory {
                        memory.merge(kind_memory)?;
                    }
                }
                types.insert(key);
            }
        }
    }
    Ok((types, set_memory, nullable))
}

fn contains_runtime_type(
    types: &RuntimeTypes,
    key: &(u16, String),
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<bool> {
    if let Some(control) = control {
        control.check()?;
    }
    let exists = types.contains(key);
    if let Some(control) = control {
        control.check()?;
    }
    Ok(exists)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cypher::{VectorCoordinateType, VectorValue};
    use crate::query::resources::QueryResources;

    #[test]
    fn returned_conversion_keeps_output_lease() {
        let resources = QueryResources::new(Some(4096));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut memory = None;
        let input = [Value::List(vec![Value::String("abc".to_owned()); 4])];
        let output = evaluate_with_memory("tostringlist", &input, Some(&control), &mut memory)
            .unwrap()
            .unwrap();
        assert!(resources.used() > 0);
        drop(output);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn value_type_does_not_copy_large_payload_or_duplicate_types() {
        let resources = QueryResources::new(Some(4096));
        let control = ExecutionControl::new(&resources, &|| false);
        let input = [Value::List(vec![Value::String("x".repeat(1024)); 2048])];
        let mut memory = None;
        let output = evaluate_with_memory("valuetype", &input, Some(&control), &mut memory)
            .unwrap()
            .unwrap();
        assert_eq!(
            output,
            Value::String("LIST<STRING NOT NULL> NOT NULL".to_owned())
        );
        assert!(resources.used() > 0);
        drop(output);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn string_or_null_preserves_memory_rejection() {
        let resources = QueryResources::new(Some(64));
        let control = ExecutionControl::new(&resources, &|| false);
        let error = evaluate_controlled(
            "tostringornull",
            &[Value::String("x".repeat(1024))],
            Some(&control),
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn vector_list_conversion_has_one_output_buffer() {
        let count = 2048;
        let bytes = allocation_bytes::<Value>(count).unwrap();
        let resources = QueryResources::new(Some(bytes));
        let control = ExecutionControl::new(&resources, &|| false);
        let vector = Value::Vector(
            VectorValue::new(VectorCoordinateType::I64, VectorValues::I64(vec![7; count])).unwrap(),
        );
        let output = evaluate_controlled("tofloatlist", &[vector], Some(&control))
            .unwrap()
            .unwrap();
        assert_eq!(output, Value::List(vec![Value::Float(7.0); count]));
        assert_eq!(resources.peak(), bytes);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn list_conversion_does_not_swallow_interrupt() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| true);
        let error = evaluate_controlled(
            "tointegerlist",
            &[Value::List(vec![Value::Integer(1); 1024])],
            Some(&control),
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Interrupted);
        assert_eq!(resources.used(), 0);
    }
}
