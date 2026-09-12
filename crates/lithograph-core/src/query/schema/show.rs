use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{
    AstKind, AstNode, IndexKind, NodeValue, RelationshipValue, ShowConstraintFilterKind,
    ShowTargetKind, Value,
};
use crate::storage::{
    ConstraintDefinition, ConstraintDefinitionKind, IndexDefinition, IndexTarget, PropertyRule,
    PropertyType, SchemaState, SchemaTarget, Snapshot, StandardIndexKind,
};

use super::super::expression::{BindingRow, BindingValue};
use super::super::{QueryError, QueryResult};

pub(crate) fn show_rows(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    clause: &AstNode,
) -> QueryResult<Vec<BindingRow>> {
    let schema = SchemaState::load(connection, snapshot.commit())?;
    let target = clause
        .descendants()
        .find_map(|node| match node.kind {
            AstKind::ShowTarget(target) => Some(target),
            _ => None,
        })
        .ok_or_else(|| QueryError::semantic("SHOW is missing its target"))?;
    match target {
        ShowTargetKind::CurrentGraphType => graph_type_rows(&schema, clause),
        ShowTargetKind::Indexes => index_rows(&schema, clause),
        ShowTargetKind::Constraints => constraint_rows(&schema, clause),
        ShowTargetKind::Functions | ShowTargetKind::Procedures => Err(QueryError::internal(
            "function/procedure SHOW reached the Schema SHOW executor",
        )),
    }
}

fn graph_type_rows(schema: &SchemaState, clause: &AstNode) -> QueryResult<Vec<BindingRow>> {
    if clause
        .descendants()
        .any(|node| node.kind == AstKind::ShowAsGraph)
    {
        let (nodes, relationships) = graph_type_as_graph(schema)?;
        return Ok(vec![binding_row([
            ("nodes", Value::List(nodes)),
            ("relationships", Value::List(relationships)),
        ])]);
    }
    Ok(vec![binding_row([(
        "specification",
        Value::String(graph_type_specification(schema)),
    )])])
}

fn graph_type_as_graph(schema: &SchemaState) -> QueryResult<(Vec<Value>, Vec<Value>)> {
    let mut next_id = -1_i64;
    let (mut nodes, element_ids, label_ids) = graph_type_node_values(schema, &mut next_id);
    let (any_nodes, any_ids, independent_relationship_types) =
        graph_type_any_nodes(schema, &mut next_id);
    nodes.extend(any_nodes);

    let mut relationships = implied_label_values(schema, &element_ids, &label_ids, &mut next_id);
    relationships.extend(relationship_element_values(
        schema,
        &element_ids,
        &label_ids,
        &any_ids,
        &mut next_id,
    )?);
    relationships.extend(independent_relationship_type_values(
        schema,
        &independent_relationship_types,
        &any_ids,
        &mut next_id,
    )?);
    Ok((nodes, relationships))
}

fn graph_type_node_values(
    schema: &SchemaState,
    next_id: &mut i64,
) -> (
    Vec<Value>,
    BTreeMap<String, String>,
    BTreeMap<String, String>,
) {
    let mut nodes = Vec::new();
    let mut element_ids = BTreeMap::new();
    for definition in schema.graph_nodes.values() {
        let element_id = virtual_id(next_id);
        element_ids.insert(definition.label.clone(), element_id.clone());
        nodes.push(Value::Node(NodeValue {
            element_id,
            labels: vec!["NodeElementType".to_owned()],
            properties: BTreeMap::from([
                ("label".to_owned(), Value::String(definition.label.clone())),
                (
                    "properties".to_owned(),
                    property_spec_list(&definition.properties),
                ),
                (
                    "constraints".to_owned(),
                    string_list(constraint_specs_for_target(
                        schema,
                        &SchemaTarget::Node {
                            label: definition.label.clone(),
                        },
                        true,
                        ConstraintFormat::Virtual,
                    )),
                ),
            ]),
        }));
    }

    let mut label_ids = BTreeMap::new();
    for label in graph_type_node_labels(schema) {
        let element_id = virtual_id(next_id);
        label_ids.insert(label.clone(), element_id.clone());
        nodes.push(Value::Node(NodeValue {
            element_id,
            labels: vec!["NodeLabel".to_owned()],
            properties: BTreeMap::from([
                ("label".to_owned(), Value::String(label.clone())),
                (
                    "constraints".to_owned(),
                    string_list(constraint_specs_for_target(
                        schema,
                        &SchemaTarget::Node { label },
                        false,
                        ConstraintFormat::Virtual,
                    )),
                ),
            ]),
        }));
    }
    (nodes, element_ids, label_ids)
}

fn graph_type_node_labels(schema: &SchemaState) -> BTreeSet<String> {
    let mut labels = BTreeSet::new();
    for definition in schema.graph_nodes.values() {
        labels.extend(definition.implied_labels.iter().cloned());
    }
    for definition in schema.graph_relationships.values() {
        if let Some(label) = &definition.source_label
            && !definition.source_identifying
        {
            labels.insert(label.clone());
        }
        if let Some(label) = &definition.target_label
            && !definition.target_identifying
        {
            labels.insert(label.clone());
        }
    }
    for constraint in schema.constraints.values() {
        if let SchemaTarget::Node { label } = &constraint.target
            && !schema.graph_nodes.contains_key(label)
        {
            labels.insert(label.clone());
        }
    }
    labels
}

fn graph_type_any_nodes(
    schema: &SchemaState,
    next_id: &mut i64,
) -> (Vec<Value>, BTreeMap<String, String>, BTreeSet<String>) {
    let mut keys = BTreeSet::new();
    for definition in schema.graph_relationships.values() {
        if definition.source_label.is_none() || definition.target_label.is_none() {
            keys.insert(format!("element/{}", definition.relationship_type));
        }
    }
    let independent_relationship_types = schema
        .constraints
        .values()
        .filter_map(|constraint| match &constraint.target {
            SchemaTarget::Relationship { relationship_type }
                if !schema.graph_relationships.contains_key(relationship_type) =>
            {
                Some(relationship_type.clone())
            }
            _ => None,
        })
        .collect::<BTreeSet<_>>();
    keys.extend(
        independent_relationship_types
            .iter()
            .map(|relationship_type| format!("constraint/{relationship_type}")),
    );
    let mut nodes = Vec::new();
    let mut ids = BTreeMap::new();
    for key in keys {
        let element_id = virtual_id(next_id);
        ids.insert(key, element_id.clone());
        nodes.push(Value::Node(NodeValue {
            element_id,
            labels: vec!["AnyLabel".to_owned()],
            properties: BTreeMap::new(),
        }));
    }
    (nodes, ids, independent_relationship_types)
}

fn implied_label_values(
    schema: &SchemaState,
    element_ids: &BTreeMap<String, String>,
    label_ids: &BTreeMap<String, String>,
    next_id: &mut i64,
) -> Vec<Value> {
    let mut relationships = Vec::new();
    for definition in schema.graph_nodes.values() {
        let Some(start) = element_ids.get(&definition.label).cloned() else {
            continue;
        };
        for implied in &definition.implied_labels {
            if let Some(end) = label_ids.get(implied).cloned() {
                relationships.push(Value::Relationship(RelationshipValue {
                    element_id: virtual_id(next_id),
                    relationship_type: "IMPLIES".to_owned(),
                    start: start.clone(),
                    end,
                    properties: BTreeMap::new(),
                }));
            }
        }
    }
    relationships
}

fn relationship_element_values(
    schema: &SchemaState,
    element_ids: &BTreeMap<String, String>,
    label_ids: &BTreeMap<String, String>,
    any_ids: &BTreeMap<String, String>,
    next_id: &mut i64,
) -> QueryResult<Vec<Value>> {
    let mut relationships = Vec::new();
    for definition in schema.graph_relationships.values() {
        let any_key = format!("element/{}", definition.relationship_type);
        let start = virtual_endpoint_id(
            definition.source_label.as_deref(),
            definition.source_identifying,
            element_ids,
            label_ids,
        )
        .or_else(|| any_ids.get(&any_key).cloned())
        .ok_or_else(|| QueryError::internal("Graph Type virtual source node is missing"))?;
        let end = virtual_endpoint_id(
            definition.target_label.as_deref(),
            definition.target_identifying,
            element_ids,
            label_ids,
        )
        .or_else(|| any_ids.get(&any_key).cloned())
        .ok_or_else(|| QueryError::internal("Graph Type virtual target node is missing"))?;
        relationships.push(Value::Relationship(RelationshipValue {
            element_id: virtual_id(next_id),
            relationship_type: "RELATIONSHIP_ELEMENT_TYPE".to_owned(),
            start,
            end,
            properties: BTreeMap::from([
                (
                    "relationshipType".to_owned(),
                    Value::String(definition.relationship_type.clone()),
                ),
                (
                    "properties".to_owned(),
                    property_spec_list(&definition.properties),
                ),
                (
                    "constraints".to_owned(),
                    string_list(constraint_specs_for_target(
                        schema,
                        &SchemaTarget::Relationship {
                            relationship_type: definition.relationship_type.clone(),
                        },
                        true,
                        ConstraintFormat::Virtual,
                    )),
                ),
            ]),
        }));
    }
    Ok(relationships)
}

fn independent_relationship_type_values(
    schema: &SchemaState,
    relationship_types: &BTreeSet<String>,
    any_ids: &BTreeMap<String, String>,
    next_id: &mut i64,
) -> QueryResult<Vec<Value>> {
    let mut relationships = Vec::new();
    for relationship_type in relationship_types {
        let any = any_ids
            .get(&format!("constraint/{relationship_type}"))
            .cloned()
            .ok_or_else(|| QueryError::internal("Graph Type virtual AnyLabel node is missing"))?;
        relationships.push(Value::Relationship(RelationshipValue {
            element_id: virtual_id(next_id),
            relationship_type: "RELATIONSHIP_TYPE".to_owned(),
            start: any.clone(),
            end: any,
            properties: BTreeMap::from([
                (
                    "relationshipType".to_owned(),
                    Value::String(relationship_type.clone()),
                ),
                (
                    "constraints".to_owned(),
                    string_list(constraint_specs_for_target(
                        schema,
                        &SchemaTarget::Relationship {
                            relationship_type: relationship_type.clone(),
                        },
                        false,
                        ConstraintFormat::Virtual,
                    )),
                ),
            ]),
        }));
    }
    Ok(relationships)
}

fn virtual_endpoint_id(
    label: Option<&str>,
    identifying: bool,
    element_ids: &BTreeMap<String, String>,
    label_ids: &BTreeMap<String, String>,
) -> Option<String> {
    let label = label?;
    if identifying {
        element_ids.get(label).cloned()
    } else {
        label_ids.get(label).cloned()
    }
}

fn virtual_id(next_id: &mut i64) -> String {
    let id = next_id.to_string();
    *next_id -= 1;
    id
}

fn property_spec_list(properties: &BTreeMap<String, PropertyRule>) -> Value {
    string_list(
        properties
            .iter()
            .map(|(name, rule)| {
                format!(
                    "{} :: {}",
                    display_identifier(name),
                    format_property_rule(rule)
                )
            })
            .collect(),
    )
}

fn index_rows(schema: &SchemaState, clause: &AstNode) -> QueryResult<Vec<BindingRow>> {
    let requested_kind = clause.descendants().find_map(|node| match node.kind {
        AstKind::IndexKind(kind) => Some(kind),
        _ => None,
    });
    let mut rows = Vec::new();
    for (ordinal, index) in schema.indexes.values().enumerate() {
        if !index_kind_matches(requested_kind, index.kind) {
            continue;
        }
        let (entity_type, labels_or_types, properties) = index_target_columns(&index.target);
        rows.push(binding_map(BTreeMap::from([
            ("id".to_owned(), Value::Integer((ordinal + 1) as i64)),
            ("name".to_owned(), Value::String(index.name.clone())),
            ("state".to_owned(), Value::String("ONLINE".to_owned())),
            ("populationPercent".to_owned(), Value::Float(100.0)),
            (
                "type".to_owned(),
                Value::String(index_kind_name(index.kind).to_owned()),
            ),
            (
                "entityType".to_owned(),
                Value::String(entity_type.to_owned()),
            ),
            ("labelsOrTypes".to_owned(), string_list(labels_or_types)),
            ("properties".to_owned(), string_list(properties)),
            (
                "indexProvider".to_owned(),
                Value::String("lithograph-standard-1.0".to_owned()),
            ),
            (
                "owningConstraint".to_owned(),
                index
                    .owning_constraint
                    .as_ref()
                    .map_or(Value::Null, |name| Value::String(name.clone())),
            ),
            ("lastRead".to_owned(), Value::Null),
            ("readCount".to_owned(), Value::Integer(0)),
            ("trackedSince".to_owned(), Value::Null),
            ("options".to_owned(), Value::Map(BTreeMap::new())),
            ("failureMessage".to_owned(), Value::String(String::new())),
            (
                "createStatement".to_owned(),
                Value::String(index_create_statement(index)),
            ),
        ])));
    }
    Ok(rows)
}

fn constraint_rows(schema: &SchemaState, clause: &AstNode) -> QueryResult<Vec<BindingRow>> {
    let filter = clause.descendants().find_map(|node| match node.kind {
        AstKind::ShowConstraintFilter(filter) => Some(filter),
        _ => None,
    });
    let mut rows = Vec::new();
    for (ordinal, constraint) in constraint_views(schema).into_iter().enumerate() {
        if !constraint_filter_matches(filter, &constraint.kind) {
            continue;
        }
        rows.push(binding_map(BTreeMap::from([
            ("id".to_owned(), Value::Integer((ordinal + 1) as i64)),
            ("name".to_owned(), Value::String(constraint.name)),
            ("type".to_owned(), Value::String(constraint.kind)),
            (
                "entityType".to_owned(),
                Value::String(constraint.entity_type),
            ),
            (
                "labelsOrTypes".to_owned(),
                string_list(constraint.labels_or_types),
            ),
            (
                "properties".to_owned(),
                constraint.properties.map_or(Value::Null, string_list),
            ),
            (
                "enforcedLabel".to_owned(),
                constraint.enforced_label.map_or(Value::Null, Value::String),
            ),
            (
                "classification".to_owned(),
                Value::String(constraint.classification),
            ),
            (
                "ownedIndex".to_owned(),
                constraint.owned_index.map_or(Value::Null, Value::String),
            ),
            (
                "propertyType".to_owned(),
                constraint.property_type.map_or(Value::Null, Value::String),
            ),
            (
                "options".to_owned(),
                constraint.options.unwrap_or(Value::Null),
            ),
            (
                "createStatement".to_owned(),
                constraint
                    .create_statement
                    .map_or(Value::Null, Value::String),
            ),
        ])));
    }
    Ok(rows)
}

fn constraint_filter_matches(filter: Option<ShowConstraintFilterKind>, kind: &str) -> bool {
    let Some(filter) = filter else {
        return true;
    };
    match filter {
        ShowConstraintFilterKind::All => true,
        ShowConstraintFilterKind::NodeUnique => kind == "NODE_PROPERTY_UNIQUENESS",
        ShowConstraintFilterKind::RelationshipUnique => kind == "RELATIONSHIP_PROPERTY_UNIQUENESS",
        ShowConstraintFilterKind::Unique => matches!(
            kind,
            "NODE_PROPERTY_UNIQUENESS" | "RELATIONSHIP_PROPERTY_UNIQUENESS"
        ),
        ShowConstraintFilterKind::NodePropertyExistence => kind == "NODE_PROPERTY_EXISTENCE",
        ShowConstraintFilterKind::RelationshipPropertyExistence => {
            kind == "RELATIONSHIP_PROPERTY_EXISTENCE"
        }
        ShowConstraintFilterKind::PropertyExistence => matches!(
            kind,
            "NODE_PROPERTY_EXISTENCE" | "RELATIONSHIP_PROPERTY_EXISTENCE"
        ),
        ShowConstraintFilterKind::NodeExistence => {
            matches!(kind, "NODE_PROPERTY_EXISTENCE" | "NODE_LABEL_EXISTENCE")
        }
        ShowConstraintFilterKind::RelationshipExistence => {
            kind == "RELATIONSHIP_PROPERTY_EXISTENCE"
        }
        ShowConstraintFilterKind::Existence => matches!(
            kind,
            "NODE_PROPERTY_EXISTENCE" | "RELATIONSHIP_PROPERTY_EXISTENCE" | "NODE_LABEL_EXISTENCE"
        ),
        ShowConstraintFilterKind::NodePropertyType => kind == "NODE_PROPERTY_TYPE",
        ShowConstraintFilterKind::RelationshipPropertyType => kind == "RELATIONSHIP_PROPERTY_TYPE",
        ShowConstraintFilterKind::PropertyType => {
            matches!(kind, "NODE_PROPERTY_TYPE" | "RELATIONSHIP_PROPERTY_TYPE")
        }
        ShowConstraintFilterKind::NodeKey => kind == "NODE_KEY",
        ShowConstraintFilterKind::RelationshipKey => kind == "RELATIONSHIP_KEY",
        ShowConstraintFilterKind::Key => matches!(kind, "NODE_KEY" | "RELATIONSHIP_KEY"),
    }
}

#[derive(Debug, Clone)]
struct ConstraintView {
    name: String,
    kind: String,
    entity_type: String,
    labels_or_types: Vec<String>,
    properties: Option<Vec<String>>,
    enforced_label: Option<String>,
    classification: String,
    owned_index: Option<String>,
    property_type: Option<String>,
    options: Option<Value>,
    create_statement: Option<String>,
}

fn constraint_views(schema: &SchemaState) -> Vec<ConstraintView> {
    let mut views = Vec::new();
    for constraint in schema.constraints.values() {
        views.push(constraint_view(constraint));
        if let ConstraintDefinitionKind::Type { rule } = &constraint.kind
            && rule.required
            && constraint.origin.is_some()
        {
            views.push(required_property_view(constraint));
        }
    }
    for definition in schema.graph_nodes.values() {
        for implied in &definition.implied_labels {
            views.push(ConstraintView {
                name: generated_constraint_name(&format!(
                    "graph/node/{}/implied/{implied}",
                    definition.label
                )),
                kind: "NODE_LABEL_EXISTENCE".to_owned(),
                entity_type: "NODE".to_owned(),
                labels_or_types: vec![definition.label.clone()],
                properties: None,
                enforced_label: Some(implied.clone()),
                classification: "dependent".to_owned(),
                owned_index: None,
                property_type: None,
                options: None,
                create_statement: None,
            });
        }
    }
    for definition in schema.graph_relationships.values() {
        if let Some(source_label) = &definition.source_label {
            views.push(relationship_endpoint_view(
                &definition.relationship_type,
                source_label,
                true,
            ));
        }
        if let Some(target_label) = &definition.target_label {
            views.push(relationship_endpoint_view(
                &definition.relationship_type,
                target_label,
                false,
            ));
        }
    }
    views.sort_by(|left, right| left.name.cmp(&right.name));
    views
}

fn constraint_view(constraint: &ConstraintDefinition) -> ConstraintView {
    let (entity_type, labels_or_types) = constraint_target_columns(&constraint.target);
    let key_or_unique = is_key_or_unique(constraint);
    let dependent = constraint.origin.is_some() && !key_or_unique;
    let classification = if key_or_unique {
        "undesignated"
    } else if dependent {
        "dependent"
    } else {
        "independent"
    };
    let kind = match (&constraint.target, &constraint.kind) {
        (SchemaTarget::Node { .. }, ConstraintDefinitionKind::Key) => "NODE_KEY",
        (SchemaTarget::Relationship { .. }, ConstraintDefinitionKind::Key) => "RELATIONSHIP_KEY",
        (SchemaTarget::Node { .. }, ConstraintDefinitionKind::Unique) => "NODE_PROPERTY_UNIQUENESS",
        (SchemaTarget::Relationship { .. }, ConstraintDefinitionKind::Unique) => {
            "RELATIONSHIP_PROPERTY_UNIQUENESS"
        }
        (SchemaTarget::Node { .. }, ConstraintDefinitionKind::NotNull) => "NODE_PROPERTY_EXISTENCE",
        (SchemaTarget::Relationship { .. }, ConstraintDefinitionKind::NotNull) => {
            "RELATIONSHIP_PROPERTY_EXISTENCE"
        }
        (SchemaTarget::Node { .. }, ConstraintDefinitionKind::Type { .. }) => "NODE_PROPERTY_TYPE",
        (SchemaTarget::Relationship { .. }, ConstraintDefinitionKind::Type { .. }) => {
            "RELATIONSHIP_PROPERTY_TYPE"
        }
    };
    let property_type = match &constraint.kind {
        ConstraintDefinitionKind::Type { rule } => Some(format_property_type(&rule.property_type)),
        _ => None,
    };
    ConstraintView {
        name: constraint.name.clone(),
        kind: kind.to_owned(),
        entity_type: entity_type.to_owned(),
        labels_or_types,
        properties: Some(constraint.properties.clone()),
        enforced_label: None,
        classification: classification.to_owned(),
        owned_index: key_or_unique.then(|| constraint.name.clone()),
        property_type,
        options: key_or_unique.then(index_backed_constraint_options),
        create_statement: (!dependent).then(|| constraint_create_statement(constraint)),
    }
}

fn required_property_view(constraint: &ConstraintDefinition) -> ConstraintView {
    let (entity_type, labels_or_types) = constraint_target_columns(&constraint.target);
    let kind = match constraint.target {
        SchemaTarget::Node { .. } => "NODE_PROPERTY_EXISTENCE",
        SchemaTarget::Relationship { .. } => "RELATIONSHIP_PROPERTY_EXISTENCE",
    };
    ConstraintView {
        name: generated_constraint_name(&format!(
            "{}|required|{:?}|{:?}",
            constraint.name, constraint.target, constraint.properties
        )),
        kind: kind.to_owned(),
        entity_type: entity_type.to_owned(),
        labels_or_types,
        properties: Some(constraint.properties.clone()),
        enforced_label: None,
        classification: "dependent".to_owned(),
        owned_index: None,
        property_type: None,
        options: None,
        create_statement: None,
    }
}

fn relationship_endpoint_view(
    relationship_type: &str,
    enforced_label: &str,
    source: bool,
) -> ConstraintView {
    ConstraintView {
        name: generated_constraint_name(&format!(
            "graph/relationship/{relationship_type}/{}/{enforced_label}",
            if source { "source" } else { "target" }
        )),
        kind: if source {
            "RELATIONSHIP_SOURCE_LABEL"
        } else {
            "RELATIONSHIP_TARGET_LABEL"
        }
        .to_owned(),
        entity_type: "RELATIONSHIP".to_owned(),
        labels_or_types: vec![relationship_type.to_owned()],
        properties: None,
        enforced_label: Some(enforced_label.to_owned()),
        classification: "dependent".to_owned(),
        owned_index: None,
        property_type: None,
        options: None,
        create_statement: None,
    }
}

fn index_backed_constraint_options() -> Value {
    Value::Map(BTreeMap::from([(
        "indexConfig".to_owned(),
        Value::Map(BTreeMap::new()),
    )]))
}

fn generated_constraint_name(canonical: &str) -> String {
    let hash = blake3::hash(canonical.as_bytes()).to_hex();
    format!("constraint_{}", &hash.as_str()[..16])
}

fn graph_type_specification(schema: &SchemaState) -> String {
    let mut entries = Vec::new();
    for definition in schema.graph_nodes.values() {
        let properties = format_properties(&definition.properties);
        let implied = if definition.implied_labels.is_empty() {
            String::new()
        } else {
            format!(
                " :{}",
                definition
                    .implied_labels
                    .iter()
                    .map(|label| quote_identifier(label))
                    .collect::<Vec<_>>()
                    .join("&")
            )
        };
        entries.push(format!(
            "(:{} =>{}{})",
            quote_identifier(&definition.label),
            implied,
            properties
        ));
    }
    for definition in schema.graph_relationships.values() {
        let properties = format_properties(&definition.properties);
        entries.push(format!(
            "{}-[:{} =>{}]->{}",
            graph_type_endpoint(
                definition.source_label.as_deref(),
                definition.source_identifying,
            ),
            quote_identifier(&definition.relationship_type),
            properties,
            graph_type_endpoint(
                definition.target_label.as_deref(),
                definition.target_identifying,
            )
        ));
    }
    for constraint in schema
        .constraints
        .values()
        .filter(|constraint| constraint.origin.is_none() || is_key_or_unique(constraint))
    {
        entries.push(constraint_specification(
            schema,
            constraint,
            ConstraintFormat::Canonical,
        ));
    }
    if entries.is_empty() {
        "{}".to_owned()
    } else {
        format!("{{\r\n  {}\r\n}}", entries.join(",\r\n  "))
    }
}

fn graph_type_endpoint(label: Option<&str>, identifying: bool) -> String {
    let Some(label) = label else {
        return "()".to_owned();
    };
    let identifying = if identifying { " =>" } else { "" };
    format!("(:{}{identifying})", quote_identifier(label))
}

#[derive(Clone, Copy)]
enum ConstraintFormat {
    Canonical,
    Virtual,
}

fn constraint_specs_for_target(
    schema: &SchemaState,
    target: &SchemaTarget,
    key_unique_only: bool,
    format: ConstraintFormat,
) -> Vec<String> {
    schema
        .constraints
        .values()
        .filter(|constraint| {
            &constraint.target == target && (!key_unique_only || is_key_or_unique(constraint))
        })
        .map(|constraint| constraint_specification(schema, constraint, format))
        .collect()
}

fn constraint_specification(
    schema: &SchemaState,
    constraint: &ConstraintDefinition,
    format: ConstraintFormat,
) -> String {
    let (name, variable, pattern) = match (&constraint.target, format) {
        (SchemaTarget::Node { label }, ConstraintFormat::Canonical) => (
            quote_identifier(&constraint.name),
            "`n`",
            format!(
                "(`n`:{}{})",
                quote_identifier(label),
                if schema.graph_nodes.contains_key(label) {
                    " =>"
                } else {
                    ""
                }
            ),
        ),
        (SchemaTarget::Relationship { relationship_type }, ConstraintFormat::Canonical) => (
            quote_identifier(&constraint.name),
            "`r`",
            format!(
                "()-[`r`:{}{}]->()",
                quote_identifier(relationship_type),
                if schema.graph_relationships.contains_key(relationship_type) {
                    " =>"
                } else {
                    ""
                }
            ),
        ),
        (SchemaTarget::Node { label }, ConstraintFormat::Virtual) => (
            display_identifier(&constraint.name),
            "n",
            format!(
                "(n:{}{})",
                display_identifier(label),
                if schema.graph_nodes.contains_key(label) {
                    " =>"
                } else {
                    ""
                }
            ),
        ),
        (SchemaTarget::Relationship { relationship_type }, ConstraintFormat::Virtual) => (
            display_identifier(&constraint.name),
            "r",
            format!(
                "()-[r:{}{}]->()",
                display_identifier(relationship_type),
                if schema.graph_relationships.contains_key(relationship_type) {
                    " =>"
                } else {
                    ""
                }
            ),
        ),
    };
    format!(
        "CONSTRAINT {name} FOR {pattern} REQUIRE {}",
        constraint_requirement_formatted(constraint, variable, format)
    )
}

fn is_key_or_unique(constraint: &ConstraintDefinition) -> bool {
    matches!(
        constraint.kind,
        ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique
    )
}

fn format_properties(properties: &BTreeMap<String, PropertyRule>) -> String {
    if properties.is_empty() {
        return String::new();
    }
    format!(
        " {{{}}}",
        properties
            .iter()
            .map(|(name, rule)| format!(
                "{} :: {}",
                quote_identifier(name),
                format_property_rule(rule)
            ))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn format_property_rule(rule: &PropertyRule) -> String {
    let mut text = format_property_type(&rule.property_type);
    if rule.required {
        text.push_str(" NOT NULL");
    }
    text
}

fn format_property_type(property_type: &PropertyType) -> String {
    match property_type {
        PropertyType::Any => "ANY".to_owned(),
        PropertyType::Boolean => "BOOLEAN".to_owned(),
        PropertyType::Integer => "INTEGER".to_owned(),
        PropertyType::Float => "FLOAT".to_owned(),
        PropertyType::String => "STRING".to_owned(),
        PropertyType::Date => "DATE".to_owned(),
        PropertyType::LocalTime => "LOCAL TIME".to_owned(),
        PropertyType::ZonedTime => "ZONED TIME".to_owned(),
        PropertyType::LocalDateTime => "LOCAL DATETIME".to_owned(),
        PropertyType::ZonedDateTime => "ZONED DATETIME".to_owned(),
        PropertyType::Duration => "DURATION".to_owned(),
        PropertyType::Point => "POINT".to_owned(),
        PropertyType::Uuid => "UUID".to_owned(),
        PropertyType::Vector {
            coordinate,
            dimension,
        } => format!("VECTOR<{coordinate}, {dimension}>"),
        PropertyType::List { element } => {
            format!("LIST<{} NOT NULL>", format_property_type(element))
        }
        PropertyType::Union { members } => members
            .iter()
            .map(format_property_type)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

fn index_kind_matches(requested: Option<IndexKind>, actual: StandardIndexKind) -> bool {
    match requested {
        None => true,
        Some(IndexKind::Lookup) => actual == StandardIndexKind::Lookup,
        Some(IndexKind::Range) => actual == StandardIndexKind::Range,
        Some(IndexKind::Text) => actual == StandardIndexKind::Text,
        Some(IndexKind::Point) => actual == StandardIndexKind::Point,
        Some(IndexKind::FullText | IndexKind::Vector) => false,
    }
}

fn index_kind_name(kind: StandardIndexKind) -> &'static str {
    match kind {
        StandardIndexKind::Lookup => "LOOKUP",
        StandardIndexKind::Range => "RANGE",
        StandardIndexKind::Text => "TEXT",
        StandardIndexKind::Point => "POINT",
    }
}

fn index_target_columns(target: &IndexTarget) -> (&'static str, Vec<String>, Vec<String>) {
    match target {
        IndexTarget::NodeLookup => ("NODE", Vec::new(), Vec::new()),
        IndexTarget::RelationshipLookup => ("RELATIONSHIP", Vec::new(), Vec::new()),
        IndexTarget::NodeProperties { label, properties } => {
            ("NODE", vec![label.clone()], properties.clone())
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => (
            "RELATIONSHIP",
            vec![relationship_type.clone()],
            properties.clone(),
        ),
    }
}

fn constraint_target_columns(target: &SchemaTarget) -> (&'static str, Vec<String>) {
    match target {
        SchemaTarget::Node { label } => ("NODE", vec![label.clone()]),
        SchemaTarget::Relationship { relationship_type } => {
            ("RELATIONSHIP", vec![relationship_type.clone()])
        }
    }
}

fn index_create_statement(index: &IndexDefinition) -> String {
    let name = quote_identifier(&index.name);
    match &index.target {
        IndexTarget::NodeLookup => format!("CREATE LOOKUP INDEX {name} FOR (n) ON EACH labels(n)"),
        IndexTarget::RelationshipLookup => {
            format!("CREATE LOOKUP INDEX {name} FOR ()-[r]-() ON EACH type(r)")
        }
        IndexTarget::NodeProperties { label, properties } => format!(
            "CREATE {} INDEX {name} FOR (n:{}) ON ({})",
            index_kind_name(index.kind),
            quote_identifier(label),
            properties
                .iter()
                .map(|property| format!("n.{}", quote_identifier(property)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => format!(
            "CREATE {} INDEX {name} FOR ()-[r:{}]-() ON ({})",
            index_kind_name(index.kind),
            quote_identifier(relationship_type),
            properties
                .iter()
                .map(|property| format!("r.{}", quote_identifier(property)))
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

fn constraint_create_statement(constraint: &ConstraintDefinition) -> String {
    format!(
        "CREATE CONSTRAINT {} FOR {} REQUIRE {}",
        quote_identifier(&constraint.name),
        constraint_pattern(constraint),
        constraint_requirement(constraint)
    )
}

fn constraint_pattern(constraint: &ConstraintDefinition) -> String {
    match &constraint.target {
        SchemaTarget::Node { label } => format!("(n:{})", quote_identifier(label)),
        SchemaTarget::Relationship { relationship_type } => {
            format!("()-[r:{}]-()", quote_identifier(relationship_type))
        }
    }
}

fn constraint_requirement(constraint: &ConstraintDefinition) -> String {
    let variable = match constraint.target {
        SchemaTarget::Node { .. } => "n",
        SchemaTarget::Relationship { .. } => "r",
    };
    constraint_requirement_with_variable(constraint, variable)
}

fn constraint_requirement_with_variable(
    constraint: &ConstraintDefinition,
    variable: &str,
) -> String {
    let properties = constraint
        .properties
        .iter()
        .map(|property| format!("{variable}.{}", quote_identifier(property)))
        .collect::<Vec<_>>();
    let expression = format!("({})", properties.join(", "));
    let property_type = match &constraint.kind {
        ConstraintDefinitionKind::Type { rule } => Some(format_property_rule(rule)),
        _ => None,
    };
    format_constraint_requirement(&constraint.kind, &expression, property_type.as_deref())
}

fn constraint_requirement_formatted(
    constraint: &ConstraintDefinition,
    variable: &str,
    format: ConstraintFormat,
) -> String {
    let properties = constraint
        .properties
        .iter()
        .map(|property| match format {
            ConstraintFormat::Canonical => format!("{variable}.{}", quote_identifier(property)),
            ConstraintFormat::Virtual => {
                format!("{variable}.{}", display_identifier(property))
            }
        })
        .collect::<Vec<_>>();
    let expression = match format {
        ConstraintFormat::Canonical => format!("({})", properties.join(", ")),
        ConstraintFormat::Virtual if properties.len() == 1 => properties[0].clone(),
        ConstraintFormat::Virtual => format!("({})", properties.join(", ")),
    };
    let property_type = match &constraint.kind {
        ConstraintDefinitionKind::Type { rule } => Some(format_property_type(&rule.property_type)),
        _ => None,
    };
    format_constraint_requirement(&constraint.kind, &expression, property_type.as_deref())
}

fn format_constraint_requirement(
    kind: &ConstraintDefinitionKind,
    expression: &str,
    property_type: Option<&str>,
) -> String {
    match kind {
        ConstraintDefinitionKind::Key => format!("{expression} IS KEY"),
        ConstraintDefinitionKind::Unique => format!("{expression} IS UNIQUE"),
        ConstraintDefinitionKind::NotNull => format!("{expression} IS NOT NULL"),
        ConstraintDefinitionKind::Type { .. } => {
            format!("{expression} IS :: {}", property_type.unwrap_or_default())
        }
    }
}

fn quote_identifier(value: &str) -> String {
    format!("`{}`", value.replace('`', "``"))
}

fn display_identifier(value: &str) -> String {
    let mut characters = value.chars();
    let simple = characters
        .next()
        .is_some_and(|character| character == '_' || character.is_ascii_alphabetic())
        && characters.all(|character| character == '_' || character.is_ascii_alphanumeric());
    if simple {
        value.to_owned()
    } else {
        quote_identifier(value)
    }
}

fn string_list(values: Vec<String>) -> Value {
    Value::List(values.into_iter().map(Value::String).collect())
}

fn binding_row<const N: usize>(values: [(&str, Value); N]) -> BindingRow {
    binding_map(
        values
            .into_iter()
            .map(|(name, value)| (name.to_owned(), value))
            .collect(),
    )
}

fn binding_map(values: BTreeMap<String, Value>) -> BindingRow {
    let mut row = BindingRow::default();
    for (name, value) in values {
        row.insert(name, BindingValue::Scalar(value));
    }
    row
}
