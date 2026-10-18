use super::*;
use serde_json::{Map, Value as JsonValue, json};

fn check(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    control.map_or(Ok(()), ExecutionControl::check)
}

fn work(control: Option<&ExecutionControl<'_>>) -> QueryResult<()> {
    control.map_or(Ok(()), |control| control.work(1))
}

fn text(value: &str, control: Option<&ExecutionControl<'_>>) -> QueryResult<String> {
    super::super::expression::clone_string_controlled(value, control)
}

fn encode_value(value: &Value, control: Option<&ExecutionControl<'_>>) -> QueryResult<JsonValue> {
    cypher::encode_json_checked(value, &mut |units, bytes| {
        let Some(control) = control else {
            return Ok(());
        };
        if units == 0 && bytes == 0 {
            return control.check();
        }
        control.work(units)?;
        control.bytes(bytes)
    })
}

fn decode_value(value: &JsonValue, control: &ExecutionControl<'_>) -> QueryResult<Value> {
    cypher::decode_json_checked(value, &mut |units| {
        if units == 0 {
            control.check()
        } else {
            control.work(units as u64)
        }
    })
}

fn stringify(value: &JsonValue, control: Option<&ExecutionControl<'_>>) -> QueryResult<String> {
    check(control)?;
    let result = serde_json::to_string(value)
        .map_err(|error| QueryError::internal(format!("failed to encode spill row: {error}")))?;
    check(control)?;
    Ok(result)
}

fn parse(text: &str, control: &ExecutionControl<'_>) -> QueryResult<JsonValue> {
    control.check()?;
    let result = serde_json::from_str(text)
        .map_err(|error| QueryError::internal(format!("failed to decode spill row: {error}")))?;
    control.check()?;
    Ok(result)
}

pub(super) fn encode_row(row: &[Value], control: &ExecutionControl<'_>) -> QueryResult<String> {
    control.check()?;
    let mut values = Vec::with_capacity(row.len());
    for value in row {
        values.push(encode_value(value, Some(control))?);
    }
    stringify(&JsonValue::Array(values), Some(control))
}

pub(super) fn encode_binding_row(
    row: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<String> {
    let mut values = Map::new();
    for (name, value) in &row.values {
        control.work(1)?;
        values.insert(
            text(name, Some(control))?,
            encode_binding_value(value, control)?,
        );
    }
    let mut used = Vec::with_capacity(row.used_relationships.len());
    for id in &row.used_relationships {
        control.work(1)?;
        used.push(json!(id));
    }
    let mut order = Vec::with_capacity(row.order.len());
    for name in &row.order {
        control.work(1)?;
        order.push(JsonValue::String(text(name, Some(control))?));
    }
    let mut object = Map::new();
    object.insert("values".to_owned(), JsonValue::Object(values));
    object.insert("usedRelationships".to_owned(), JsonValue::Array(used));
    object.insert("order".to_owned(), JsonValue::Array(order));
    object.insert(
        "loadCsvContext".to_owned(),
        encode_context(row.load_csv_context.as_ref(), control)?,
    );
    stringify(&JsonValue::Object(object), Some(control))
}

fn encode_context(
    context: Option<&LoadCsvContext>,
    control: &ExecutionControl<'_>,
) -> QueryResult<JsonValue> {
    let Some(context) = context else {
        return Ok(JsonValue::Null);
    };
    let file = context
        .file
        .as_ref()
        .map(|file| text(file, Some(control)).map(JsonValue::String))
        .transpose()?
        .unwrap_or(JsonValue::Null);
    let mut object = Map::new();
    object.insert("file".to_owned(), file);
    object.insert("line".to_owned(), json!(context.line));
    Ok(JsonValue::Object(object))
}

fn encode_binding_value(
    value: &BindingValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<JsonValue> {
    control.work(1)?;
    let (kind, payload) = match value {
        BindingValue::Node(id) => ("node", Some(("id", json!(id)))),
        BindingValue::Relationship(record) => {
            ("relationship", Some(("record", relationship_json(*record))))
        }
        BindingValue::Scalar(value) => (
            "scalar",
            Some(("value", encode_value(value, Some(control))?)),
        ),
        BindingValue::Null => ("null", None),
        BindingValue::Path {
            nodes,
            relationships,
        } => return encode_path_binding(nodes, relationships, control),
    };
    let mut object = Map::new();
    object.insert("kind".to_owned(), JsonValue::String(kind.to_owned()));
    if let Some((name, value)) = payload {
        object.insert(name.to_owned(), value);
    }
    Ok(JsonValue::Object(object))
}

fn encode_path_binding(
    nodes: &[i64],
    relationships: &[RelationshipRecord],
    control: &ExecutionControl<'_>,
) -> QueryResult<JsonValue> {
    let mut encoded_nodes = Vec::with_capacity(nodes.len());
    for id in nodes {
        control.work(1)?;
        encoded_nodes.push(json!(id));
    }
    let mut encoded_relationships = Vec::with_capacity(relationships.len());
    for record in relationships {
        control.work(1)?;
        encoded_relationships.push(relationship_json(*record));
    }
    let mut object = Map::new();
    object.insert("kind".to_owned(), JsonValue::String("path".to_owned()));
    object.insert("nodes".to_owned(), JsonValue::Array(encoded_nodes));
    object.insert(
        "relationships".to_owned(),
        JsonValue::Array(encoded_relationships),
    );
    Ok(JsonValue::Object(object))
}

fn relationship_json(record: RelationshipRecord) -> JsonValue {
    json!([record.id, record.source, record.type_id, record.target])
}

pub(super) fn decode_binding_row(
    encoded: &str,
    control: &ExecutionControl<'_>,
) -> QueryResult<BindingRow> {
    let value = parse(encoded, control)?;
    let object = value
        .as_object()
        .ok_or_else(|| QueryError::internal("binding spill row is not a JSON object"))?;
    let entries = object
        .get("values")
        .and_then(JsonValue::as_object)
        .ok_or_else(|| QueryError::internal("binding spill row has no values object"))?;
    let mut values = BTreeMap::new();
    for (name, value) in entries {
        control.work(1)?;
        values.insert(
            text(name, Some(control))?,
            decode_binding_value(value, control)?,
        );
    }
    let mut used_relationships = BTreeSet::new();
    for id in json_i64_array(
        object.get("usedRelationships"),
        "usedRelationships",
        control,
    )? {
        control.work(1)?;
        used_relationships.insert(id);
    }
    Ok(BindingRow {
        values,
        used_relationships,
        order: decode_order(object.get("order"), control)?,
        load_csv_context: decode_load_csv_context(object.get("loadCsvContext"), control)?,
    })
}

fn decode_order(
    value: Option<&JsonValue>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Vec<String>> {
    let values = value
        .and_then(JsonValue::as_array)
        .ok_or_else(|| QueryError::internal("binding spill row has no order array"))?;
    let mut order = Vec::with_capacity(values.len());
    for value in values {
        control.work(1)?;
        let value = value
            .as_str()
            .ok_or_else(|| QueryError::internal("binding spill order entry is not a String"))?;
        order.push(text(value, Some(control))?);
    }
    Ok(order)
}

fn decode_load_csv_context(
    value: Option<&JsonValue>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<LoadCsvContext>> {
    let Some(value) = value.filter(|value| !value.is_null()) else {
        return Ok(None);
    };
    let object = value
        .as_object()
        .ok_or_else(|| QueryError::internal("binding spill LOAD CSV context is not an object"))?;
    let file = object
        .get("file")
        .ok_or_else(|| QueryError::internal("binding spill LOAD CSV context has no file"))?;
    let file = if file.is_null() {
        None
    } else {
        let file = file.as_str().ok_or_else(|| {
            QueryError::internal("binding spill LOAD CSV context file is not a String")
        })?;
        Some(text(file, Some(control))?)
    };
    let line = object
        .get("line")
        .ok_or_else(|| QueryError::internal("binding spill LOAD CSV context has no line"))?;
    let line = if line.is_null() {
        None
    } else {
        Some(json_i64(Some(line), "LOAD CSV line")?)
    };
    Ok(Some(LoadCsvContext { file, line }))
}

fn decode_binding_value(
    value: &JsonValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<BindingValue> {
    control.work(1)?;
    let object = value
        .as_object()
        .ok_or_else(|| QueryError::internal("binding spill value is not a JSON object"))?;
    let kind = object
        .get("kind")
        .and_then(JsonValue::as_str)
        .ok_or_else(|| QueryError::internal("binding spill value has no kind"))?;
    match kind {
        "node" => Ok(BindingValue::Node(json_i64(object.get("id"), "node id")?)),
        "relationship" => Ok(BindingValue::Relationship(decode_relationship_json(
            object.get("record"),
        )?)),
        "path" => decode_path_binding(object, control),
        "scalar" => object
            .get("value")
            .ok_or_else(|| QueryError::internal("binding spill scalar has no value"))
            .and_then(|value| decode_value(value, control))
            .map(BindingValue::Scalar),
        "null" => Ok(BindingValue::Null),
        other => Err(QueryError::internal(format!(
            "binding spill value has unknown kind {other:?}"
        ))),
    }
}

fn decode_path_binding(
    object: &Map<String, JsonValue>,
    control: &ExecutionControl<'_>,
) -> QueryResult<BindingValue> {
    let nodes = json_i64_array(object.get("nodes"), "path nodes", control)?;
    let encoded = object
        .get("relationships")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| QueryError::internal("binding spill path has no relationships"))?;
    let mut relationships = Vec::with_capacity(encoded.len());
    for value in encoded {
        control.work(1)?;
        relationships.push(decode_relationship_json(Some(value))?);
    }
    Ok(BindingValue::Path {
        nodes,
        relationships,
    })
}

fn decode_relationship_json(value: Option<&JsonValue>) -> QueryResult<RelationshipRecord> {
    let values = value
        .and_then(JsonValue::as_array)
        .ok_or_else(|| QueryError::internal("binding spill relationship is not an array"))?;
    if values.len() != 4 {
        return Err(QueryError::internal(
            "binding spill relationship does not have four fields",
        ));
    }
    Ok(RelationshipRecord {
        id: json_i64(values.first(), "relationship id")?,
        source: json_i64(values.get(1), "relationship source")?,
        type_id: json_i64(values.get(2), "relationship type")?,
        target: json_i64(values.get(3), "relationship target")?,
    })
}

fn json_i64(value: Option<&JsonValue>, role: &str) -> QueryResult<i64> {
    value
        .and_then(JsonValue::as_i64)
        .ok_or_else(|| QueryError::internal(format!("binding spill {role} is not INTEGER64")))
}

fn json_i64_array(
    value: Option<&JsonValue>,
    role: &str,
    control: &ExecutionControl<'_>,
) -> QueryResult<Vec<i64>> {
    let values = value
        .and_then(JsonValue::as_array)
        .ok_or_else(|| QueryError::internal(format!("binding spill {role} is not an array")))?;
    let mut output = Vec::with_capacity(values.len());
    for value in values {
        control.work(1)?;
        output.push(json_i64(Some(value), role)?);
    }
    Ok(output)
}

pub(super) fn decode_row(encoded: &str, control: &ExecutionControl<'_>) -> QueryResult<Vec<Value>> {
    let value = parse(encoded, control)?;
    let array = value
        .as_array()
        .ok_or_else(|| QueryError::internal("spill row is not a JSON array"))?;
    let mut output = Vec::with_capacity(array.len());
    for value in array {
        output.push(decode_value(value, control)?);
    }
    Ok(output)
}

pub(in crate::query) fn distinct_row_key(
    row: &[Value],
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<String> {
    check(control)?;
    let mut key = Vec::with_capacity(row.len());
    for value in row {
        key.push(distinct_value_key(value, control)?);
    }
    stringify(&JsonValue::Array(key), control)
}

fn tagged_key(label: &str, value: JsonValue) -> JsonValue {
    JsonValue::Array(vec![JsonValue::String(label.to_owned()), value])
}

fn distinct_value_key(
    value: &Value,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<JsonValue> {
    work(control)?;
    let key = match value {
        Value::Null => JsonValue::Array(vec![JsonValue::String("null".to_owned())]),
        Value::Boolean(value) => tagged_key("boolean", JsonValue::Bool(*value)),
        Value::Integer(value) => tagged_key("number", JsonValue::String(format!("i:{value}"))),
        Value::Float(value) => tagged_key("number", JsonValue::String(float_distinct_key(*value))),
        Value::String(value) => tagged_key("string", JsonValue::String(text(value, control)?)),
        Value::List(values) => {
            check(control)?;
            let mut keys = Vec::with_capacity(values.len());
            for value in values {
                keys.push(distinct_value_key(value, control)?);
            }
            tagged_key("list", JsonValue::Array(keys))
        }
        Value::Map(values) => distinct_map_key(values, control)?,
        Value::Node(value) => {
            tagged_key("node", JsonValue::String(text(&value.element_id, control)?))
        }
        Value::Relationship(value) => tagged_key(
            "relationship",
            JsonValue::String(text(&value.element_id, control)?),
        ),
        Value::Path(value) => distinct_path_key(value, control)?,
        _ => tagged_key(
            "value",
            canonicalize_json_numbers(encode_value(value, control)?, control)?,
        ),
    };
    Ok(key)
}

fn distinct_map_key(
    values: &BTreeMap<String, Value>,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<JsonValue> {
    let mut entries = Vec::with_capacity(values.len());
    for (name, value) in values {
        work(control)?;
        entries.push(JsonValue::Array(vec![
            JsonValue::String(text(name, control)?),
            distinct_value_key(value, control)?,
        ]));
    }
    Ok(tagged_key("map", JsonValue::Array(entries)))
}

fn distinct_path_key(
    value: &crate::cypher::PathValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<JsonValue> {
    let mut nodes = Vec::with_capacity(value.nodes.len());
    for node in &value.nodes {
        work(control)?;
        nodes.push(JsonValue::String(text(&node.element_id, control)?));
    }
    let mut relationships = Vec::with_capacity(value.relationships.len());
    for relationship in &value.relationships {
        work(control)?;
        relationships.push(JsonValue::String(text(&relationship.element_id, control)?));
    }
    Ok(JsonValue::Array(vec![
        JsonValue::String("path".to_owned()),
        JsonValue::Array(nodes),
        JsonValue::Array(relationships),
    ]))
}

fn float_distinct_key(value: f64) -> String {
    if value.is_nan() {
        return "nan".to_owned();
    }
    if value.is_finite() && value.fract() == 0.0 {
        const I64_UPPER_EXCLUSIVE: f64 = 9_223_372_036_854_775_808.0;
        const I64_LOWER_INCLUSIVE: f64 = -9_223_372_036_854_775_808.0;
        if (I64_LOWER_INCLUSIVE..I64_UPPER_EXCLUSIVE).contains(&value) {
            return format!("i:{}", value as i64);
        }
    }
    format!("f:{:016x}", value.to_bits())
}

fn canonicalize_json_numbers(
    value: JsonValue,
    control: Option<&ExecutionControl<'_>>,
) -> QueryResult<JsonValue> {
    work(control)?;
    Ok(match value {
        JsonValue::Number(number) => number.as_f64().map_or(
            JsonValue::Number(number),
            |value| json!({"$number": float_distinct_key(value)}),
        ),
        JsonValue::Array(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(canonicalize_json_numbers(value, control)?);
            }
            JsonValue::Array(output)
        }
        JsonValue::Object(values) => {
            let mut output = Map::new();
            for (name, value) in values {
                work(control)?;
                output.insert(name, canonicalize_json_numbers(value, control)?);
            }
            JsonValue::Object(output)
        }
        value => value,
    })
}
