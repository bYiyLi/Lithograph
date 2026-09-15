use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::storage::{
    self, ConstraintDefinition, ConstraintDefinitionKind, GraphNodeType, GraphRelationshipType,
    HashId, LayerBuilder, OwnerKind, PropertyRule, PropertyValue, SchemaState, SchemaTarget,
    Snapshot,
};

use super::super::{QueryError, QueryErrorKind, QueryResult};
use super::equality::property_equality_key;

const CONSTRAINT_VALIDATION_PAGE_SIZE: usize = 4_096;

pub(crate) fn validate_layer_against_commit_schema(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
    layer: &LayerBuilder,
) -> QueryResult<()> {
    if layer.is_empty() {
        return Ok(());
    }
    let schema = SchemaState::load(connection, commit)?;
    if schema.graph_nodes.is_empty()
        && schema.graph_relationships.is_empty()
        && schema.constraints.is_empty()
    {
        return Ok(());
    }
    let impact = LayerSchemaImpact::resolve(snapshot, layer)?;
    validate_impacted_graph_nodes(connection, &schema, snapshot, &impact)?;
    validate_impacted_graph_relationships(connection, &schema, snapshot, &impact)?;
    validate_impacted_constraints(connection, &schema, snapshot, &impact)
}

fn validate_impacted_graph_nodes(
    connection: &Connection,
    schema: &SchemaState,
    snapshot: &Snapshot<'_>,
    impact: &LayerSchemaImpact,
) -> QueryResult<()> {
    for definition in schema.graph_nodes.values() {
        let Some(label_id) = storage::find_label(connection, &definition.label)? else {
            continue;
        };
        if impact.node_has_label(label_id) {
            validate_graph_node_type(connection, definition, snapshot)?;
        }
    }
    Ok(())
}

fn validate_impacted_graph_relationships(
    connection: &Connection,
    schema: &SchemaState,
    snapshot: &Snapshot<'_>,
    impact: &LayerSchemaImpact,
) -> QueryResult<()> {
    for definition in schema.graph_relationships.values() {
        let Some(type_id) =
            storage::find_relationship_type(connection, &definition.relationship_type)?
        else {
            continue;
        };
        let source_label_changed = changed_optional_label(
            connection,
            definition.source_label.as_deref(),
            &impact.changed_labels,
        )?;
        let target_label_changed = changed_optional_label(
            connection,
            definition.target_label.as_deref(),
            &impact.changed_labels,
        )?;
        if impact.relationship_types.contains(&type_id)
            || source_label_changed
            || target_label_changed
        {
            validate_graph_relationship_type(connection, definition, snapshot)?;
        }
    }
    Ok(())
}

fn validate_impacted_constraints(
    connection: &Connection,
    schema: &SchemaState,
    snapshot: &Snapshot<'_>,
    impact: &LayerSchemaImpact,
) -> QueryResult<()> {
    for constraint in schema.constraints.values() {
        let affected = match &constraint.target {
            SchemaTarget::Node { label } => storage::find_label(connection, label)?
                .is_some_and(|label_id| impact.node_has_label(label_id)),
            SchemaTarget::Relationship { relationship_type } => {
                storage::find_relationship_type(connection, relationship_type)?
                    .is_some_and(|type_id| impact.relationship_types.contains(&type_id))
            }
        };
        if affected {
            validate_constraint(connection, constraint, snapshot)?;
        }
    }
    Ok(())
}

struct LayerSchemaImpact {
    node_labels: Vec<Vec<i64>>,
    relationship_types: BTreeSet<i64>,
    changed_labels: BTreeSet<i64>,
}

impl LayerSchemaImpact {
    fn resolve(snapshot: &Snapshot<'_>, layer: &LayerBuilder) -> QueryResult<Self> {
        let mut touched_nodes = BTreeSet::new();
        let mut touched_relationships = BTreeSet::new();
        let mut relationship_types = BTreeSet::new();
        let mut changed_labels = BTreeSet::new();

        for (node_id, _) in layer.node_changes() {
            touched_nodes.insert(node_id);
        }
        for (node_id, label_id, _) in layer.label_changes() {
            touched_nodes.insert(node_id);
            changed_labels.insert(label_id);
        }
        for (relationship, added) in layer.relationship_changes() {
            if added {
                relationship_types.insert(relationship.type_id);
            }
            touched_relationships.insert(relationship.id);
        }
        for (owner, owner_id, _, _) in layer.property_changes() {
            match owner {
                OwnerKind::Node => {
                    touched_nodes.insert(owner_id);
                }
                OwnerKind::Relationship => {
                    touched_relationships.insert(owner_id);
                }
            }
        }

        let mut node_labels = Vec::with_capacity(touched_nodes.len());
        for node_id in touched_nodes {
            if snapshot.node_exists(node_id)? {
                node_labels.push(snapshot.labels(node_id)?);
            }
        }
        for relationship_id in touched_relationships {
            if let Some(relationship) = snapshot.relationship(relationship_id)? {
                relationship_types.insert(relationship.type_id);
            }
        }
        Ok(Self {
            node_labels,
            relationship_types,
            changed_labels,
        })
    }

    fn node_has_label(&self, label_id: i64) -> bool {
        self.node_labels
            .iter()
            .any(|labels| labels.binary_search(&label_id).is_ok())
    }
}

fn changed_optional_label(
    connection: &Connection,
    label: Option<&str>,
    changed_labels: &BTreeSet<i64>,
) -> QueryResult<bool> {
    let Some(label) = label else {
        return Ok(false);
    };
    Ok(storage::find_label(connection, label)?
        .is_some_and(|label_id| changed_labels.contains(&label_id)))
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

pub(crate) fn validate_schema_transition(
    connection: &Connection,
    previous: &SchemaState,
    next: &SchemaState,
    snapshot: &Snapshot<'_>,
) -> QueryResult<()> {
    for (label, definition) in &next.graph_nodes {
        if previous.graph_nodes.get(label) != Some(definition) {
            validate_graph_node_type(connection, definition, snapshot)?;
        }
    }
    for (relationship_type, definition) in &next.graph_relationships {
        if previous.graph_relationships.get(relationship_type) != Some(definition) {
            validate_graph_relationship_type(connection, definition, snapshot)?;
        }
    }
    for (name, constraint) in &next.constraints {
        if previous.constraints.get(name) != Some(constraint) {
            validate_constraint(connection, constraint, snapshot)?;
        }
    }
    Ok(())
}

pub(crate) fn validation_conflicts(
    connection: &Connection,
    schema: &SchemaState,
    snapshot: &Snapshot<'_>,
) -> QueryResult<Vec<(String, QueryError)>> {
    let mut conflicts = Vec::new();
    for (label, definition) in &schema.graph_nodes {
        collect_validation_conflict(
            storage::graph_node_slot(label),
            validate_graph_node_type(connection, definition, snapshot),
            &mut conflicts,
        )?;
    }
    for (relationship_type, definition) in &schema.graph_relationships {
        collect_validation_conflict(
            storage::graph_relationship_slot(relationship_type),
            validate_graph_relationship_type(connection, definition, snapshot),
            &mut conflicts,
        )?;
    }
    for (name, constraint) in &schema.constraints {
        collect_validation_conflict(
            storage::constraint_slot(name),
            validate_constraint(connection, constraint, snapshot),
            &mut conflicts,
        )?;
    }
    Ok(conflicts)
}

fn collect_validation_conflict(
    slot: String,
    result: QueryResult<()>,
    conflicts: &mut Vec<(String, QueryError)>,
) -> QueryResult<()> {
    match result {
        Ok(()) => Ok(()),
        Err(error) if error.kind == QueryErrorKind::Constraint => {
            conflicts.push((slot, error));
            Ok(())
        }
        Err(error) => Err(error),
    }
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
    let mut after = 0_i64;
    loop {
        let page = snapshot.scan_label_after(label_id, after, CONSTRAINT_VALIDATION_PAGE_SIZE)?;
        for node_id in page.items {
            let labels = snapshot.labels(node_id)?;
            validate_graph_node_instance(
                definition,
                snapshot,
                node_id,
                &labels,
                &implied,
                &properties,
            )?;
        }
        let Some(next_after) = page.next_after else {
            break;
        };
        after = next_after;
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
    let mut after = 0_i64;
    loop {
        let page = snapshot.scan_relationship_type_after(
            type_id,
            after,
            CONSTRAINT_VALIDATION_PAGE_SIZE,
        )?;
        for relationship in page.items {
            validate_graph_relationship_instance(
                definition,
                snapshot,
                relationship,
                source_label,
                target_label,
                &properties,
            )?;
        }
        let Some(next_after) = page.next_after else {
            break;
        };
        after = next_after;
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
    with_constraint_uniqueness_state(connection, constraint, || {
        let mut after = 0_i64;
        loop {
            let page =
                snapshot.scan_label_after(label_id, after, CONSTRAINT_VALIDATION_PAGE_SIZE)?;
            for node in page.items {
                let values = property_values(snapshot, OwnerKind::Node, node, &keys)?;
                validate_constraint_values(connection, constraint, "node", node, values)?;
            }
            let Some(next_after) = page.next_after else {
                break;
            };
            after = next_after;
        }
        Ok(())
    })
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
    with_constraint_uniqueness_state(connection, constraint, || {
        let mut after = 0_i64;
        loop {
            let page = snapshot.scan_relationships_after(after, CONSTRAINT_VALIDATION_PAGE_SIZE)?;
            for relationship in page.items {
                if relationship.type_id != type_id {
                    continue;
                }
                let values =
                    property_values(snapshot, OwnerKind::Relationship, relationship.id, &keys)?;
                validate_constraint_values(
                    connection,
                    constraint,
                    "relationship",
                    relationship.id,
                    values,
                )?;
            }
            let Some(next_after) = page.next_after else {
                break;
            };
            after = next_after;
        }
        Ok(())
    })
}

fn with_constraint_uniqueness_state(
    connection: &Connection,
    constraint: &ConstraintDefinition,
    validate: impl FnOnce() -> QueryResult<()>,
) -> QueryResult<()> {
    if !matches!(
        constraint.kind,
        ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique
    ) {
        return validate();
    }
    connection.execute_batch(
        "CREATE TEMP TABLE IF NOT EXISTS _lithograph_constraint_validation_seen(\
             key BLOB PRIMARY KEY, owner_id INTEGER NOT NULL\
         ) WITHOUT ROWID;\
         DELETE FROM temp._lithograph_constraint_validation_seen;",
    )?;
    let result = validate();
    let cleanup =
        connection.execute_batch("DROP TABLE temp._lithograph_constraint_validation_seen");
    match result {
        Ok(()) => {
            cleanup?;
            Ok(())
        }
        Err(error) => {
            let _ = cleanup;
            Err(error)
        }
    }
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
    connection: &Connection,
    constraint: &ConstraintDefinition,
    element: &str,
    element_id: i64,
    values: Vec<(&str, Option<PropertyValue>)>,
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
            if let Some(key) = uniqueness_key(constraint, element, element_id, &values)? {
                record_uniqueness_key(connection, constraint, element, element_id, &key)?;
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
) -> QueryResult<Option<Vec<u8>>> {
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
        key.push(match property_equality_key(value)? {
            Some(key) => key,
            None => non_reflexive_uniqueness_key(element_id),
        });
    }
    encode_uniqueness_key(&key).map(Some)
}

fn encode_uniqueness_key(parts: &[Vec<u8>]) -> QueryResult<Vec<u8>> {
    let mut encoded = Vec::new();
    let count = u64::try_from(parts.len())
        .map_err(|_| QueryError::internal("constraint uniqueness key has too many properties"))?;
    encoded.extend_from_slice(&count.to_le_bytes());
    for part in parts {
        let len = u64::try_from(part.len())
            .map_err(|_| QueryError::internal("constraint uniqueness key is too large"))?;
        encoded.extend_from_slice(&len.to_le_bytes());
        encoded.extend_from_slice(part);
    }
    Ok(encoded)
}

fn record_uniqueness_key(
    connection: &Connection,
    constraint: &ConstraintDefinition,
    element: &str,
    element_id: i64,
    key: &[u8],
) -> QueryResult<()> {
    let inserted = connection.execute(
        "INSERT OR IGNORE INTO temp._lithograph_constraint_validation_seen(key, owner_id) VALUES(?1, ?2)",
        rusqlite::params![key, element_id],
    )?;
    if inserted == 1 {
        return Ok(());
    }
    let existing: i64 = connection.query_row(
        "SELECT owner_id FROM temp._lithograph_constraint_validation_seen WHERE key = ?1",
        [key],
        |row| row.get(0),
    )?;
    Err(QueryError::constraint(format!(
        "constraint {} conflicts between {element} {existing} and {element} {element_id}",
        constraint.name
    )))
}

fn non_reflexive_uniqueness_key(element_id: i64) -> Vec<u8> {
    let mut key = b"non_reflexive".to_vec();
    key.extend_from_slice(&element_id.to_le_bytes());
    key
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
