use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::storage::{
    self, ConstraintDefinition, ConstraintDefinitionKind, GraphNodeType, GraphRelationshipType,
    HashId, OwnerKind, PropertyRule, PropertyValue, SchemaState, SchemaTarget, Snapshot,
};

use super::super::{QueryError, QueryResult};

pub(crate) fn validate_snapshot_against_commit_schema(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    let schema = SchemaState::load(connection, commit)?;
    validate_snapshot(connection, &schema, snapshot)
}

pub(crate) fn validate_snapshot(
    connection: &Connection,
    schema: &SchemaState,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    validate_graph_node_types(connection, schema, snapshot)?;
    validate_graph_relationship_types(connection, schema, snapshot)?;
    for constraint in schema.constraints.values() {
        validate_constraint(connection, constraint, snapshot)?;
    }
    Ok(())
}

fn validate_graph_node_types(
    connection: &Connection,
    schema: &SchemaState,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    for definition in schema.graph_nodes.values() {
        validate_graph_node_type(connection, definition, snapshot)?;
    }
    Ok(())
}

fn validate_graph_node_type(
    connection: &Connection,
    definition: &GraphNodeType,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    let Some(label_id) = storage::find_label(connection, &definition.label)? else {
        return Ok(());
    };
    let implied = definition
        .implied_labels
        .iter()
        .map(|label| Ok((label, storage::find_label(connection, label)?)))
        .collect::<QueryResult<Vec<_>>>()?;
    let properties = resolve_property_rules(connection, &definition.properties)?;
    let mut nodes = Vec::new();
    snapshot.visit_nodes(|node_id| {
        nodes.push(node_id);
        Ok(())
    })?;
    for node_id in nodes {
        let labels = snapshot.labels(node_id)?;
        if labels.contains(&label_id) {
            validate_graph_node_instance(
                definition,
                snapshot,
                node_id,
                &labels,
                &implied,
                &properties,
            )?;
        }
    }
    Ok(())
}

fn validate_graph_node_instance(
    definition: &GraphNodeType,
    snapshot: &Snapshot<'_>,
    node_id: i64,
    labels: &[i64],
    implied: &[(&String, Option<i64>)],
    properties: &[(&String, &PropertyRule, Option<i64>)],
) -> QueryResult<()> {
    for (label, implied_label_id) in implied {
        if implied_label_id.is_none_or(|id| !labels.contains(&id)) {
            return Err(QueryError::constraint(format!(
                "Graph Type node {} is missing implied label {label}",
                definition.label
            )));
        }
    }
    for (property, rule, key_id) in properties {
        let value = key_id
            .map(|key_id| snapshot.property(OwnerKind::Node, node_id, key_id))
            .transpose()?
            .flatten();
        validate_property_rule(
            &definition.label,
            "node",
            node_id,
            property,
            rule,
            value.as_ref(),
        )?;
    }
    Ok(())
}

fn validate_graph_relationship_types(
    connection: &Connection,
    schema: &SchemaState,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    for definition in schema.graph_relationships.values() {
        validate_graph_relationship_type(connection, definition, snapshot)?;
    }
    Ok(())
}

fn validate_graph_relationship_type(
    connection: &Connection,
    definition: &GraphRelationshipType,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    let Some(type_id) = storage::find_relationship_type(connection, &definition.relationship_type)?
    else {
        return Ok(());
    };
    let source_label = resolve_optional_label(connection, definition.source_label.as_deref())?;
    let target_label = resolve_optional_label(connection, definition.target_label.as_deref())?;
    let properties = resolve_property_rules(connection, &definition.properties)?;
    let mut relationships = Vec::new();
    snapshot.visit_relationships(|relationship| {
        relationships.push(relationship);
        Ok(())
    })?;
    for relationship in relationships {
        if relationship.type_id == type_id {
            validate_graph_relationship_instance(
                definition,
                snapshot,
                relationship,
                source_label,
                target_label,
                &properties,
            )?;
        }
    }
    Ok(())
}

fn resolve_optional_label<'a>(
    connection: &Connection,
    label: Option<&'a str>,
) -> QueryResult<Option<(&'a str, Option<i64>)>> {
    label
        .map(|label| Ok((label, storage::find_label(connection, label)?)))
        .transpose()
}

fn validate_graph_relationship_instance(
    definition: &GraphRelationshipType,
    snapshot: &Snapshot<'_>,
    relationship: crate::storage::RelationshipRecord,
    source_label: Option<(&str, Option<i64>)>,
    target_label: Option<(&str, Option<i64>)>,
    properties: &[(&String, &PropertyRule, Option<i64>)],
) -> QueryResult<()> {
    validate_endpoint_label(
        definition,
        snapshot,
        relationship.source,
        source_label,
        "source",
    )?;
    validate_endpoint_label(
        definition,
        snapshot,
        relationship.target,
        target_label,
        "target",
    )?;
    for (property, rule, key_id) in properties {
        let value = key_id
            .map(|key_id| snapshot.property(OwnerKind::Relationship, relationship.id, key_id))
            .transpose()?
            .flatten();
        validate_property_rule(
            &definition.relationship_type,
            "relationship",
            relationship.id,
            property,
            rule,
            value.as_ref(),
        )?;
    }
    Ok(())
}

fn validate_endpoint_label(
    definition: &GraphRelationshipType,
    snapshot: &Snapshot<'_>,
    node_id: i64,
    expected: Option<(&str, Option<i64>)>,
    endpoint: &str,
) -> QueryResult<()> {
    let Some((label, label_id)) = expected else {
        return Ok(());
    };
    let labels = snapshot.labels(node_id)?;
    if label_id.is_none_or(|id| !labels.contains(&id)) {
        return Err(QueryError::constraint(format!(
            "Graph Type relationship {} {endpoint} is missing implied label {label}",
            definition.relationship_type
        )));
    }
    Ok(())
}

fn resolve_property_rules<'a>(
    connection: &Connection,
    properties: &'a BTreeMap<String, PropertyRule>,
) -> QueryResult<Vec<(&'a String, &'a PropertyRule, Option<i64>)>> {
    properties
        .iter()
        .map(|(name, rule)| Ok((name, rule, storage::find_property_key(connection, name)?)))
        .collect()
}

fn validate_constraint(
    connection: &Connection,
    constraint: &ConstraintDefinition,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    match &constraint.target {
        SchemaTarget::Node { label } => {
            validate_node_constraint(connection, constraint, label, snapshot)
        }
        SchemaTarget::Relationship { relationship_type } => {
            validate_relationship_constraint(connection, constraint, relationship_type, snapshot)
        }
    }
}

fn validate_node_constraint(
    connection: &Connection,
    constraint: &ConstraintDefinition,
    label: &str,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    let Some(label_id) = storage::find_label(connection, label)? else {
        return Ok(());
    };
    let keys = constraint_property_keys(connection, constraint)?;
    let mut nodes = Vec::new();
    snapshot.visit_nodes(|node| {
        nodes.push(node);
        Ok(())
    })?;
    let mut seen = BTreeMap::<Vec<Vec<u8>>, i64>::new();
    for node in nodes {
        if !snapshot.labels(node)?.contains(&label_id) {
            continue;
        }
        let values = property_values(snapshot, OwnerKind::Node, node, &keys)?;
        validate_constraint_values(constraint, "node", node, values, &mut seen)?;
    }
    Ok(())
}

fn validate_relationship_constraint(
    connection: &Connection,
    constraint: &ConstraintDefinition,
    relationship_type: &str,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    let Some(type_id) = storage::find_relationship_type(connection, relationship_type)? else {
        return Ok(());
    };
    let keys = constraint_property_keys(connection, constraint)?;
    let mut relationships = Vec::new();
    snapshot.visit_relationships(|relationship| {
        relationships.push(relationship);
        Ok(())
    })?;
    let mut seen = BTreeMap::<Vec<Vec<u8>>, i64>::new();
    for relationship in relationships {
        if relationship.type_id != type_id {
            continue;
        }
        let values = property_values(snapshot, OwnerKind::Relationship, relationship.id, &keys)?;
        validate_constraint_values(
            constraint,
            "relationship",
            relationship.id,
            values,
            &mut seen,
        )?;
    }
    Ok(())
}

fn constraint_property_keys<'a>(
    connection: &Connection,
    constraint: &'a ConstraintDefinition,
) -> QueryResult<Vec<(&'a String, Option<i64>)>> {
    constraint
        .properties
        .iter()
        .map(|property| Ok((property, storage::find_property_key(connection, property)?)))
        .collect()
}

fn property_values<'a>(
    snapshot: &Snapshot<'_>,
    owner_kind: OwnerKind,
    owner_id: i64,
    keys: &'a [(&'a String, Option<i64>)],
) -> QueryResult<Vec<(&'a str, Option<PropertyValue>)>> {
    let mut values = Vec::with_capacity(keys.len());
    for (property, key_id) in keys {
        let value = key_id
            .map(|key_id| snapshot.property(owner_kind, owner_id, key_id))
            .transpose()?
            .flatten();
        values.push((property.as_str(), value));
    }
    Ok(values)
}

fn validate_constraint_values(
    constraint: &ConstraintDefinition,
    element: &str,
    element_id: i64,
    values: Vec<(&str, Option<PropertyValue>)>,
    seen: &mut BTreeMap<Vec<Vec<u8>>, i64>,
) -> QueryResult<()> {
    match &constraint.kind {
        ConstraintDefinitionKind::NotNull => {
            let (property, value) = values
                .first()
                .ok_or_else(|| QueryError::internal("NOT NULL constraint has no property"))?;
            if value.is_none() {
                return Err(QueryError::constraint(format!(
                    "constraint {} requires {element} {element_id} property {property}",
                    constraint.name
                )));
            }
        }
        ConstraintDefinitionKind::Type { rule } => {
            let (property, value) = values
                .first()
                .ok_or_else(|| QueryError::internal("property type constraint has no property"))?;
            validate_property_rule(
                &constraint.name,
                element,
                element_id,
                property,
                rule,
                value.as_ref(),
            )?;
        }
        ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique => {
            let key = uniqueness_key(constraint, element, element_id, &values)?;
            if let Some(key) = key
                && let Some(existing) = seen.insert(key, element_id)
            {
                return Err(QueryError::constraint(format!(
                    "constraint {} conflicts between {element} {existing} and {element} {element_id}",
                    constraint.name
                )));
            }
        }
    }
    Ok(())
}

fn uniqueness_key(
    constraint: &ConstraintDefinition,
    element: &str,
    element_id: i64,
    values: &[(&str, Option<PropertyValue>)],
) -> QueryResult<Option<Vec<Vec<u8>>>> {
    let require_all = matches!(constraint.kind, ConstraintDefinitionKind::Key);
    let mut key = Vec::with_capacity(values.len());
    for (property, value) in values {
        let Some(value) = value else {
            if require_all {
                return Err(QueryError::constraint(format!(
                    "constraint {} requires {element} {element_id} property {property}",
                    constraint.name
                )));
            }
            return Ok(None);
        };
        key.push(value.canonical_bytes()?);
    }
    Ok(Some(key))
}

fn validate_property_rule(
    owner: &str,
    element: &str,
    element_id: i64,
    property: &str,
    rule: &storage::PropertyRule,
    value: Option<&PropertyValue>,
) -> QueryResult<()> {
    let Some(value) = value else {
        if rule.required {
            return Err(QueryError::constraint(format!(
                "{owner} requires {element} {element_id} property {property}"
            )));
        }
        return Ok(());
    };
    if !rule.accepts(value) {
        return Err(QueryError::constraint(format!(
            "{owner} rejects the type of {element} {element_id} property {property}"
        )));
    }
    Ok(())
}
