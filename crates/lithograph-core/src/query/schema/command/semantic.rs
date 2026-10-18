use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{AstKind, AstNode, ClauseKind, Value};
use crate::query::expression::{self, BindingRow, compile_expression, surface_expressions};
use crate::storage::{
    HashId, IndexConfiguration, IndexDefinition, IndexTarget, SchemaState, StandardIndexKind,
};

use super::super::super::QueryResult;
use super::{insert_index_definition, schema_error};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum SemanticCreateKind {
    Node,
    Relationship,
}

pub(super) fn create_call(root: &AstNode) -> QueryResult<Option<(&AstNode, SemanticCreateKind)>> {
    let mut found = Vec::new();
    for clause in root
        .descendants()
        .filter(|node| node.kind == AstKind::Clause(ClauseKind::Call))
    {
        let Some(name) = clause
            .descendants()
            .find(|node| node.kind == AstKind::FunctionName)
            .and_then(|node| node.text.as_deref())
        else {
            continue;
        };
        let kind = match name.to_ascii_lowercase().as_str() {
            "db.index.semantic.createnodeindex" => SemanticCreateKind::Node,
            "db.index.semantic.createrelationshipindex" => SemanticCreateKind::Relationship,
            _ => continue,
        };
        found.push((clause, kind));
    }
    if found.is_empty() {
        return Ok(None);
    }
    if found.len() != 1
        || root
            .descendants()
            .filter(|node| matches!(node.kind, AstKind::Clause(_)))
            .count()
            != 1
    {
        return Err(schema_error(
            "Semantic Index create procedure must be a standalone CALL",
        ));
    }
    let (clause, kind) = found[0];
    if clause
        .descendants()
        .any(|node| matches!(node.kind, AstKind::YieldItem | AstKind::YieldAll))
    {
        return Err(schema_error(
            "Semantic Index create procedure does not produce YIELD columns",
        ));
    }
    Ok(Some((clause, kind)))
}

pub(super) fn apply_create_index(
    connection: &Connection,
    base_commit: HashId,
    state: &mut SchemaState,
    clause: &AstNode,
    kind: SemanticCreateKind,
    execution: &super::SchemaExecution<'_>,
) -> QueryResult<()> {
    let (arguments, _arguments_memory) =
        create_arguments(connection, base_commit, clause, execution)?;
    if arguments.len() != 4 {
        return Err(schema_error(
            "Semantic Index create procedure requires exactly 4 arguments",
        ));
    }
    let control = execution.control;
    // Definition construction may simultaneously retain labels, a uniqueness
    // tree and a JSON Provider configuration alongside the evaluated arguments.
    let _definition_memory = control.reserve(crate::query::resources::checked_mul(
        _arguments_memory.bytes(),
        4,
    )?)?;
    let name = required_nonempty_string(&arguments[0], "indexName")?;
    let labels_or_types = required_string_list(
        &arguments[1],
        match kind {
            SemanticCreateKind::Node => "labels",
            SemanticCreateKind::Relationship => "relationshipTypes",
        },
        control,
    )?;
    let source_property = required_nonempty_string(&arguments[2], "sourceProperty")?;
    let Value::Map(options) = &arguments[3] else {
        return Err(schema_error("Semantic Index options must be a MAP"));
    };
    let configuration = parse_options(options, control)?;
    let target = match kind {
        SemanticCreateKind::Node => IndexTarget::NodeProperties {
            label: labels_or_types[0].clone(),
            properties: vec![source_property],
        },
        SemanticCreateKind::Relationship => IndexTarget::RelationshipProperties {
            relationship_type: labels_or_types[0].clone(),
            properties: vec![source_property],
        },
    };
    insert_index_definition(
        state,
        IndexDefinition {
            name,
            kind: StandardIndexKind::Semantic,
            target,
            owning_constraint: None,
            labels_or_types,
            additional_properties: Vec::new(),
            configuration: Some(configuration),
        },
        false,
    )
}

pub(super) fn validate_definition(index: &IndexDefinition) -> QueryResult<()> {
    let name = &index.name;
    if index.owning_constraint.is_some() {
        return Err(schema_error(format!(
            "Semantic Index {name} cannot be owned by a Constraint"
        )));
    }
    let mut targets = BTreeSet::new();
    if index.labels_or_types.iter().any(|target| {
        target.is_empty() || target.contains(char::from(0)) || !targets.insert(target.as_str())
    }) {
        return Err(schema_error(format!(
            "Semantic Index {name} requires unique non-empty labels or Relationship Types"
        )));
    }
    let primary_target = match &index.target {
        IndexTarget::NodeProperties { label, properties } => {
            validate_source_properties(name, properties)?;
            label
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => {
            validate_source_properties(name, properties)?;
            relationship_type
        }
        IndexTarget::NodeLookup | IndexTarget::RelationshipLookup => {
            return Err(schema_error(format!(
                "Semantic Index {name} requires a property target"
            )));
        }
    };
    if index.labels_or_types.first() != Some(primary_target) {
        return Err(schema_error(format!(
            "Semantic Index {name} target must match its first label or Relationship Type"
        )));
    }
    let Some(IndexConfiguration::Semantic {
        provider,
        provider_config,
        dimensions,
        similarity_function,
    }) = index.configuration.as_ref()
    else {
        return Err(schema_error(format!(
            "Semantic Index {name} requires Semantic configuration"
        )));
    };
    if provider.is_empty() || provider.contains(char::from(0)) {
        return Err(schema_error(format!(
            "Semantic Index {name} provider must be a non-empty STRING"
        )));
    }
    if !provider_config.is_object() {
        return Err(schema_error(format!(
            "Semantic Index {name} providerConfig must be a MAP"
        )));
    }
    if !(1..=4096).contains(dimensions) {
        return Err(schema_error(format!(
            "Semantic Index {name} dimensions must be between 1 and 4096"
        )));
    }
    if !matches!(similarity_function.as_str(), "cosine" | "euclidean") {
        return Err(schema_error(format!(
            "Semantic Index {name} similarity must be 'cosine' or 'euclidean'"
        )));
    }
    Ok(())
}

fn create_arguments(
    connection: &Connection,
    base_commit: HashId,
    clause: &AstNode,
    execution: &super::SchemaExecution<'_>,
) -> QueryResult<(Vec<Value>, crate::query::MemoryReservation)> {
    let control = execution.control;
    let name_end = clause
        .descendants()
        .find(|node| node.kind == AstKind::FunctionName)
        .map_or(clause.span.start, |node| node.span.end);
    let arguments = clause
        .descendants()
        .find(|node| node.kind == AstKind::ArgumentList && node.span.start >= name_end)
        .ok_or_else(|| schema_error("Semantic Index create procedure is missing arguments"))?;
    let snapshot =
        crate::query::graph::resolve_snapshot_with_control(connection, base_commit, control)?;
    let row = BindingRow::default();
    let expressions = surface_expressions(arguments);
    let mut memory = control.reserve(crate::query::resources::allocation_bytes::<Value>(
        expressions.len(),
    )?)?;
    let mut values = Vec::with_capacity(expressions.len());
    for node in expressions {
        let expression = compile_expression(node)?;
        let evaluated = expression::evaluate_with_access_control(
            &expression,
            &snapshot,
            &row,
            execution.params,
            execution.query_access,
            control,
        )?;
        memory.merge(evaluated.reservation)?;
        values.push(evaluated.value);
    }
    Ok((values, memory))
}

fn parse_options(
    options: &BTreeMap<String, Value>,
    control: &crate::query::ExecutionControl<'_>,
) -> QueryResult<IndexConfiguration> {
    let supported = BTreeSet::from(["provider", "providerConfig", "dimensions", "similarity"]);
    if let Some(key) = options.keys().find(|key| !supported.contains(key.as_str())) {
        return Err(schema_error(format!(
            "unsupported Semantic Index option {key:?}"
        )));
    }
    let provider = required_nonempty_string(
        options
            .get("provider")
            .ok_or_else(|| schema_error("Semantic Index options require provider"))?,
        "provider",
    )?;
    let provider_config = options
        .get("providerConfig")
        .ok_or_else(|| schema_error("Semantic Index options require providerConfig"))?;
    let Value::Map(_) = provider_config else {
        return Err(schema_error("Semantic Index providerConfig must be a MAP"));
    };
    let provider_config = json_value(provider_config, control)?;
    let dimensions = match options.get("dimensions") {
        Some(Value::Integer(value)) if (1..=4096).contains(value) => *value as u64,
        Some(_) => {
            return Err(schema_error(
                "Semantic Index dimensions must be an INTEGER between 1 and 4096",
            ));
        }
        None => return Err(schema_error("Semantic Index options require dimensions")),
    };
    let similarity = required_nonempty_string(
        options
            .get("similarity")
            .ok_or_else(|| schema_error("Semantic Index options require similarity"))?,
        "similarity",
    )?
    .to_ascii_lowercase();
    if !matches!(similarity.as_str(), "cosine" | "euclidean") {
        return Err(schema_error(
            "Semantic Index similarity must be 'cosine' or 'euclidean'",
        ));
    }
    Ok(IndexConfiguration::Semantic {
        provider,
        provider_config,
        dimensions,
        similarity_function: similarity,
    })
}

fn json_value(
    value: &Value,
    control: &crate::query::ExecutionControl<'_>,
) -> QueryResult<serde_json::Value> {
    control.work(1)?;
    match value {
        Value::Null => Ok(serde_json::Value::Null),
        Value::Boolean(value) => Ok((*value).into()),
        Value::Integer(value) => Ok((*value).into()),
        Value::Float(value) if value.is_finite() => serde_json::Number::from_f64(*value)
            .map(serde_json::Value::Number)
            .ok_or_else(|| {
                schema_error("Semantic Index providerConfig must contain finite numbers")
            }),
        Value::String(value) => Ok(clone_text(value, control)?.into()),
        Value::List(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(json_value(value, control)?);
            }
            Ok(serde_json::Value::Array(output))
        }
        Value::Map(values) => {
            let mut output = serde_json::Map::new();
            for (key, value) in values {
                output.insert(clone_text(key, control)?, json_value(value, control)?);
            }
            Ok(serde_json::Value::Object(output))
        }
        _ => Err(schema_error(
            "Semantic Index providerConfig must contain only JSON-compatible values",
        )),
    }
}

fn clone_text(value: &str, control: &crate::query::ExecutionControl<'_>) -> QueryResult<String> {
    control.check()?;
    let mut output = String::with_capacity(value.len());
    let mut offset = 0;
    while offset < value.len() {
        let mut end = offset.saturating_add(64 * 1024).min(value.len());
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        let chunk = &value[offset..end];
        output.push_str(chunk);
        control.bytes(chunk.len() as u64)?;
        offset = end;
    }
    control.check()?;
    Ok(output)
}

fn required_nonempty_string(value: &Value, role: &str) -> QueryResult<String> {
    match value {
        Value::String(value) if !value.is_empty() && !value.contains(char::from(0)) => {
            Ok(value.clone())
        }
        Value::String(_) => Err(schema_error(format!(
            "Semantic Index {role} must be a non-empty STRING without NUL"
        ))),
        _ => Err(schema_error(format!(
            "Semantic Index {role} must be a STRING"
        ))),
    }
}

fn required_string_list(
    value: &Value,
    role: &str,
    control: &crate::query::ExecutionControl<'_>,
) -> QueryResult<Vec<String>> {
    let Value::List(values) = value else {
        return Err(schema_error(format!(
            "Semantic Index {role} must be a non-empty LIST<STRING>"
        )));
    };
    if values.is_empty() {
        return Err(schema_error(format!(
            "Semantic Index {role} must be a non-empty LIST<STRING>"
        )));
    }
    let mut result = Vec::with_capacity(values.len());
    let mut unique = BTreeSet::new();
    for value in values {
        control.work(1)?;
        let value = required_nonempty_string(value, role)?;
        if !unique.insert(value.clone()) {
            return Err(schema_error(format!(
                "Semantic Index {role} must not contain duplicate values"
            )));
        }
        result.push(value);
    }
    Ok(result)
}

fn validate_source_properties(name: &str, properties: &[String]) -> QueryResult<()> {
    if properties.len() != 1 || properties[0].is_empty() || properties[0].contains(char::from(0)) {
        return Err(schema_error(format!(
            "Semantic Index {name} requires exactly one non-empty source Property"
        )));
    }
    Ok(())
}
