use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{AstKind, AstNode, ClauseKind, Value};
use crate::query::expression::{self, BindingRow, compile_expression, surface_expressions};
use crate::storage::{
    HashId, IndexConfiguration, IndexDefinition, IndexTarget, SchemaState, Snapshot,
    StandardIndexKind,
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
    params: &BTreeMap<String, Value>,
) -> QueryResult<()> {
    let arguments = create_arguments(connection, base_commit, clause, params)?;
    if arguments.len() != 4 {
        return Err(schema_error(
            "Semantic Index create procedure requires exactly 4 arguments",
        ));
    }
    let name = required_nonempty_string(&arguments[0], "indexName")?;
    let labels_or_types = required_string_list(
        &arguments[1],
        match kind {
            SemanticCreateKind::Node => "labels",
            SemanticCreateKind::Relationship => "relationshipTypes",
        },
    )?;
    let source_property = required_nonempty_string(&arguments[2], "sourceProperty")?;
    let Value::Map(options) = &arguments[3] else {
        return Err(schema_error("Semantic Index options must be a MAP"));
    };
    let configuration = parse_options(options)?;
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
    params: &BTreeMap<String, Value>,
) -> QueryResult<Vec<Value>> {
    let name_end = clause
        .descendants()
        .find(|node| node.kind == AstKind::FunctionName)
        .map_or(clause.span.start, |node| node.span.end);
    let arguments = clause
        .descendants()
        .find(|node| node.kind == AstKind::ArgumentList && node.span.start >= name_end)
        .ok_or_else(|| schema_error("Semantic Index create procedure is missing arguments"))?;
    let snapshot = Snapshot::resolve(connection, base_commit)?;
    let row = BindingRow::default();
    surface_expressions(arguments)
        .into_iter()
        .map(|node| {
            let expression = compile_expression(node)?;
            expression::evaluate(&expression, &snapshot, &row, params)
        })
        .collect()
}

fn parse_options(options: &BTreeMap<String, Value>) -> QueryResult<IndexConfiguration> {
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
    let provider_config = json_value(provider_config)?;
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

fn json_value(value: &Value) -> QueryResult<serde_json::Value> {
    crate::query::version::value_to_json(value).map_err(|_| {
        schema_error("Semantic Index providerConfig must contain only JSON-compatible values")
    })
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

fn required_string_list(value: &Value, role: &str) -> QueryResult<Vec<String>> {
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
