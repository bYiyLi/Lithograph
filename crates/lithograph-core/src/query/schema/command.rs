use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::cypher::{
    self, AstKind, AstNode, ClauseKind, ConstraintKind, ExistenceModifierKind,
    GraphTypeOperationKind, IndexKind, QueryAst,
};
use crate::storage::{
    ConstraintDefinition, ConstraintDefinitionKind, GraphNodeType, GraphRelationshipType, HashId,
    IndexDefinition, IndexTarget, PropertyRule, SchemaState, SchemaTarget, StandardIndexKind,
};

use super::super::options::writable_branch;
use super::super::{ExecutionOptions, QueryError, QueryErrorKind, QueryResult};

mod property_type;

use property_type::parse_property_rule;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct SchemaCounters {
    pub constraints_added: u64,
    pub constraints_removed: u64,
    pub indexes_added: u64,
    pub indexes_removed: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedSchema {
    pub(crate) branch: String,
    pub(crate) author: Option<String>,
    pub(crate) message: Option<String>,
    pub(crate) state: SchemaState,
    pub(crate) counters: SchemaCounters,
    pub(crate) kind: ClauseKind,
}

pub(crate) fn prepare_schema(
    connection: &Connection,
    base_commit: HashId,
    ast: &QueryAst,
    source: &str,
    options: &ExecutionOptions,
) -> QueryResult<Option<PreparedSchema>> {
    let clauses = ast
        .root
        .descendants()
        .filter(|node| matches!(node.kind, AstKind::Clause(kind) if is_schema_ddl(kind)))
        .collect::<Vec<_>>();
    if clauses.is_empty() {
        return Ok(None);
    }
    if clauses.len() != 1 {
        return Err(schema_error(
            "schema DDL must contain exactly one schema command",
        ));
    }
    if !options.graph_view.is_full_graph() {
        return Err(QueryError::invalid_argument(
            "schema DDL cannot execute with options.graphView",
        ));
    }
    let branch = writable_branch(options)?;
    let clause = clauses[0];
    let AstKind::Clause(kind) = clause.kind else {
        return Err(QueryError::internal(
            "schema command is missing its clause kind",
        ));
    };
    let current = SchemaState::load(connection, base_commit)?;
    let mut state = current.clone();
    match kind {
        ClauseKind::GraphType => apply_graph_type(&mut state, clause, source)?,
        ClauseKind::CreateConstraint => apply_create_constraint(&mut state, clause, source)?,
        ClauseKind::DropConstraint => apply_drop_constraint(&mut state, clause)?,
        ClauseKind::CreateIndex => apply_create_index(&mut state, clause)?,
        ClauseKind::DropIndex => apply_drop_index(&mut state, clause)?,
        _ => return Err(QueryError::internal("unexpected schema command kind")),
    }
    refresh_backing_indexes(&mut state)?;
    validate_schema_state(&state)?;
    let counters = schema_counters(&current, &state);
    Ok(Some(PreparedSchema {
        branch,
        author: options.author.clone(),
        message: options.message.clone(),
        state,
        counters,
        kind,
    }))
}

fn is_schema_ddl(kind: ClauseKind) -> bool {
    matches!(
        kind,
        ClauseKind::GraphType
            | ClauseKind::CreateConstraint
            | ClauseKind::DropConstraint
            | ClauseKind::CreateIndex
            | ClauseKind::DropIndex
    )
}

fn schema_error(message: impl Into<String>) -> QueryError {
    QueryError::new(QueryErrorKind::Schema, message)
}

fn schema_counters(before: &SchemaState, after: &SchemaState) -> SchemaCounters {
    SchemaCounters {
        constraints_added: after
            .constraints
            .keys()
            .filter(|name| !before.constraints.contains_key(*name))
            .count() as u64,
        constraints_removed: before
            .constraints
            .keys()
            .filter(|name| !after.constraints.contains_key(*name))
            .count() as u64,
        indexes_added: after
            .indexes
            .keys()
            .filter(|name| !before.indexes.contains_key(*name))
            .count() as u64,
        indexes_removed: before
            .indexes
            .keys()
            .filter(|name| !after.indexes.contains_key(*name))
            .count() as u64,
    }
}

fn apply_graph_type(state: &mut SchemaState, clause: &AstNode, source: &str) -> QueryResult<()> {
    let operation = clause
        .descendants()
        .find_map(|node| match node.kind {
            AstKind::GraphTypeOperation(operation) => Some(operation),
            _ => None,
        })
        .ok_or_else(|| schema_error("Graph Type command is missing its operation"))?;
    let entries = parse_graph_entries(clause, source)?;
    validate_graph_entries_for_operation(operation, &entries)?;
    match operation {
        GraphTypeOperationKind::Set => {
            state.graph_nodes.clear();
            state.graph_relationships.clear();
            state.constraints.clear();
            for entry in entries {
                insert_graph_entry(state, entry, true)?;
            }
        }
        GraphTypeOperationKind::Add => {
            for entry in entries {
                insert_graph_entry(state, entry, true)?;
            }
        }
        GraphTypeOperationKind::Alter => {
            for entry in entries {
                alter_graph_entry(state, entry)?;
            }
        }
        GraphTypeOperationKind::Drop => {
            for entry in entries {
                drop_graph_entry(state, entry)?;
            }
        }
    }
    Ok(())
}

fn validate_graph_entries_for_operation(
    operation: GraphTypeOperationKind,
    entries: &[GraphEntry],
) -> QueryResult<()> {
    if operation == GraphTypeOperationKind::Drop {
        return Ok(());
    }
    for entry in entries {
        match entry {
            GraphEntry::Node { definition, .. }
                if definition.implied_labels.is_empty() && definition.properties.is_empty() =>
            {
                return Err(schema_error(format!(
                    "Graph Node Type {} must imply at least one label or define at least one property",
                    definition.label
                )));
            }
            GraphEntry::Relationship { definition, .. }
                if definition.source_label.is_none()
                    && definition.target_label.is_none()
                    && definition.properties.is_empty() =>
            {
                return Err(schema_error(format!(
                    "Graph Relationship Type {} must constrain an endpoint or define at least one property",
                    definition.relationship_type
                )));
            }
            GraphEntry::ConstraintName(_) => {
                return Err(schema_error(
                    "a Graph Type constraint name reference is only valid with DROP",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

#[derive(Debug, Clone)]
enum GraphEntry {
    Node {
        definition: GraphNodeType,
        constraints: Vec<ConstraintDefinition>,
    },
    Relationship {
        definition: GraphRelationshipType,
        constraints: Vec<ConstraintDefinition>,
    },
    Constraint(ConstraintDefinition),
    ConstraintName(String),
}

fn parse_graph_entries(clause: &AstNode, source: &str) -> QueryResult<Vec<GraphEntry>> {
    let mut entries = Vec::new();
    for node in clause.descendants() {
        match node.kind {
            AstKind::GraphNodeType => entries.push(parse_graph_node(node, source)?),
            AstKind::GraphRelationshipType => entries.push(parse_graph_relationship(node, source)?),
            AstKind::GraphConstraint => {
                if node
                    .descendants()
                    .any(|child| child.kind == AstKind::ConstraintRequirement)
                {
                    entries.push(GraphEntry::Constraint(parse_constraint_definition(
                        node, source, None,
                    )?));
                } else {
                    entries.push(GraphEntry::ConstraintName(named_descendant(
                        node,
                        AstKind::ConstraintName,
                    )?));
                }
            }
            _ => {}
        }
    }
    Ok(entries)
}

fn parse_graph_node(node: &AstNode, _source: &str) -> QueryResult<GraphEntry> {
    let labels = node
        .descendants()
        .filter(|child| child.kind == AstKind::LabelName)
        .filter_map(|child| child.text.as_deref())
        .map(cypher::unescape_identifier)
        .collect::<Vec<_>>();
    let label = labels
        .first()
        .cloned()
        .ok_or_else(|| schema_error("Graph Node Type is missing its label"))?;
    let origin = format!("graph/node/{label}");
    let target = SchemaTarget::Node {
        label: label.clone(),
    };
    let (properties, mut constraints) =
        parse_graph_properties(node, &origin, &target, &format!("Graph Node Type {label}"))?;
    constraints.extend(parse_graph_require_constraints(node, &origin, target)?);
    Ok(GraphEntry::Node {
        definition: GraphNodeType {
            label,
            implied_labels: labels.into_iter().skip(1).collect(),
            properties,
        },
        constraints,
    })
}

fn parse_graph_relationship(node: &AstNode, _source: &str) -> QueryResult<GraphEntry> {
    let relationship_node = node
        .descendants()
        .find(|child| child.kind == AstKind::RelationshipTypeName)
        .ok_or_else(|| schema_error("Graph Relationship Type is missing its type"))?;
    let relationship_type =
        cypher::unescape_identifier(relationship_node.text.as_deref().unwrap_or_default());
    let mut endpoints = node
        .descendants()
        .filter(|child| child.kind == AstKind::GraphEndpoint)
        .collect::<Vec<_>>();
    endpoints.sort_by_key(|endpoint| endpoint.span.start);
    let (source_label, source_identifying) = endpoints
        .first()
        .map(|endpoint| parse_graph_endpoint(endpoint))
        .unwrap_or((None, false));
    let (target_label, target_identifying) = endpoints
        .get(1)
        .map(|endpoint| parse_graph_endpoint(endpoint))
        .unwrap_or((None, false));
    let origin = format!("graph/relationship/{relationship_type}");
    let target = SchemaTarget::Relationship {
        relationship_type: relationship_type.clone(),
    };
    let (properties, mut constraints) = parse_graph_properties(
        node,
        &origin,
        &target,
        &format!("Graph Relationship Type {relationship_type}"),
    )?;
    constraints.extend(parse_graph_require_constraints(node, &origin, target)?);
    Ok(GraphEntry::Relationship {
        definition: GraphRelationshipType {
            relationship_type,
            source_label,
            source_identifying,
            target_label,
            target_identifying,
            properties,
        },
        constraints,
    })
}

fn parse_graph_properties(
    node: &AstNode,
    origin: &str,
    target: &SchemaTarget,
    owner: &str,
) -> QueryResult<(BTreeMap<String, PropertyRule>, Vec<ConstraintDefinition>)> {
    let mut properties = BTreeMap::new();
    let mut constraints = Vec::new();
    for property in node
        .descendants()
        .filter(|child| child.kind == AstKind::GraphProperty)
    {
        let name = first_property_key(property)?;
        let type_expression = property
            .descendants()
            .find(|child| child.kind == AstKind::TypeExpression)
            .ok_or_else(|| schema_error(format!("Graph property {name} is missing its type")))?;
        let rule = parse_property_rule(type_expression)?;
        if properties.insert(name.clone(), rule.clone()).is_some() {
            return Err(schema_error(format!(
                "{owner} defines property {name} more than once"
            )));
        }
        constraints.push(type_constraint(origin, target.clone(), name.clone(), rule));
        if let Some(kind) = property.descendants().find_map(constraint_kind) {
            constraints.push(key_or_unique_constraint(
                origin,
                target.clone(),
                vec![name],
                kind,
            )?);
        }
    }
    Ok((properties, constraints))
}

fn parse_graph_endpoint(endpoint: &AstNode) -> (Option<String>, bool) {
    let label = endpoint
        .descendants()
        .find(|child| child.kind == AstKind::LabelName)
        .and_then(|child| child.text.as_deref())
        .map(cypher::unescape_identifier);
    let identifying = endpoint
        .descendants()
        .any(|child| child.kind == AstKind::GraphImplies);
    (label, identifying)
}

fn parse_graph_require_constraints(
    node: &AstNode,
    origin: &str,
    target: SchemaTarget,
) -> QueryResult<Vec<ConstraintDefinition>> {
    let graph_properties = node
        .descendants()
        .filter(|child| child.kind == AstKind::GraphProperty)
        .map(|property| (property.span.start, property.span.end))
        .collect::<Vec<_>>();
    let mut constraints = Vec::new();
    for container in node
        .descendants()
        .filter(|child| matches!(child.kind, AstKind::ConstraintKind(_)))
    {
        let Some(kind) = constraint_kind(container) else {
            continue;
        };
        let inside_property = graph_properties
            .iter()
            .any(|(start, end)| *start <= container.span.start && container.span.end <= *end);
        if inside_property {
            continue;
        }
        let properties = property_keys(container);
        if properties.is_empty() {
            continue;
        }
        constraints.push(key_or_unique_constraint(
            origin,
            target.clone(),
            properties,
            kind,
        )?);
    }
    Ok(constraints)
}

fn insert_graph_entry(
    state: &mut SchemaState,
    entry: GraphEntry,
    reject_existing: bool,
) -> QueryResult<()> {
    match entry {
        GraphEntry::Node {
            definition,
            constraints,
        } => {
            let key = definition.label.clone();
            if reject_existing && state.graph_nodes.contains_key(&key) {
                return Err(schema_error(format!(
                    "Graph Node Type {key} already exists"
                )));
            }
            state.graph_nodes.insert(key, definition);
            insert_constraints(state, constraints, reject_existing)?;
        }
        GraphEntry::Relationship {
            definition,
            constraints,
        } => {
            let key = definition.relationship_type.clone();
            if reject_existing && state.graph_relationships.contains_key(&key) {
                return Err(schema_error(format!(
                    "Graph Relationship Type {key} already exists"
                )));
            }
            state.graph_relationships.insert(key, definition);
            insert_constraints(state, constraints, reject_existing)?;
        }
        GraphEntry::Constraint(constraint) => {
            insert_constraint(state, constraint, reject_existing)?;
        }
        GraphEntry::ConstraintName(_) => {
            return Err(schema_error(
                "a Graph Type constraint name reference is only valid with DROP",
            ));
        }
    }
    Ok(())
}

fn alter_graph_entry(state: &mut SchemaState, entry: GraphEntry) -> QueryResult<()> {
    match entry {
        GraphEntry::Node {
            definition,
            constraints,
        } => {
            let key = definition.label.clone();
            validate_graph_alter_entry(
                state.graph_nodes.contains_key(&key),
                "Graph Node Type",
                &key,
                &constraints,
            )?;
            let origin = format!("graph/node/{key}");
            remove_origin_dependent_constraints(state, &origin);
            state.graph_nodes.insert(key, definition);
            insert_constraints(state, constraints, false)?;
        }
        GraphEntry::Relationship {
            definition,
            constraints,
        } => {
            let key = definition.relationship_type.clone();
            validate_graph_alter_entry(
                state.graph_relationships.contains_key(&key),
                "Graph Relationship Type",
                &key,
                &constraints,
            )?;
            let origin = format!("graph/relationship/{key}");
            remove_origin_dependent_constraints(state, &origin);
            state.graph_relationships.insert(key, definition);
            insert_constraints(state, constraints, false)?;
        }
        GraphEntry::Constraint(_) | GraphEntry::ConstraintName(_) => {
            return Err(schema_error(
                "ALTER CURRENT GRAPH TYPE ALTER does not alter standalone constraints",
            ));
        }
    }
    Ok(())
}

fn validate_graph_alter_entry(
    exists: bool,
    kind: &str,
    key: &str,
    constraints: &[ConstraintDefinition],
) -> QueryResult<()> {
    if !exists {
        return Err(schema_error(format!("{kind} {key} does not exist")));
    }
    if constraints.iter().any(is_key_or_unique) {
        return Err(schema_error(
            "ALTER CURRENT GRAPH TYPE ALTER cannot include KEY or UNIQUE constraints",
        ));
    }
    Ok(())
}

fn drop_graph_entry(state: &mut SchemaState, entry: GraphEntry) -> QueryResult<()> {
    match entry {
        GraphEntry::Node { definition, .. } => drop_graph_node_type(state, definition)?,
        GraphEntry::Relationship { definition, .. } => {
            drop_graph_relationship_type(state, definition)?;
        }
        GraphEntry::Constraint(constraint) => {
            let name = find_equivalent_constraint_name(state, &constraint)
                .ok_or_else(|| schema_error("Graph Type constraint does not exist"))?;
            drop_named_constraint(state, &name)?;
        }
        GraphEntry::ConstraintName(name) => drop_named_constraint(state, &name)?,
    }
    Ok(())
}

fn drop_graph_node_type(state: &mut SchemaState, definition: GraphNodeType) -> QueryResult<()> {
    let key = definition.label.clone();
    let Some(existing) = state.graph_nodes.get(&key) else {
        return Err(schema_error(format!(
            "Graph Node Type {key} does not exist"
        )));
    };
    let full_definition =
        !definition.implied_labels.is_empty() || !definition.properties.is_empty();
    if full_definition && existing != &definition {
        return Err(schema_error(format!(
            "Graph Node Type {key} DROP definition does not exactly match the current definition"
        )));
    }
    if graph_node_type_is_referenced(state, &key) {
        return Err(schema_error(format!(
            "Graph Node Type {key} is referenced by a Graph Relationship Type"
        )));
    }
    state.graph_nodes.remove(&key);
    detach_origin_constraints(state, &format!("graph/node/{key}"));
    Ok(())
}

fn graph_node_type_is_referenced(state: &SchemaState, label: &str) -> bool {
    state.graph_relationships.values().any(|relationship| {
        (relationship.source_identifying && relationship.source_label.as_deref() == Some(label))
            || (relationship.target_identifying
                && relationship.target_label.as_deref() == Some(label))
    })
}

fn drop_graph_relationship_type(
    state: &mut SchemaState,
    definition: GraphRelationshipType,
) -> QueryResult<()> {
    let key = definition.relationship_type.clone();
    let Some(existing) = state.graph_relationships.get(&key) else {
        return Err(schema_error(format!(
            "Graph Relationship Type {key} does not exist"
        )));
    };
    let full_definition = definition.source_label.is_some()
        || definition.source_identifying
        || definition.target_label.is_some()
        || definition.target_identifying
        || !definition.properties.is_empty();
    if full_definition && existing != &definition {
        return Err(schema_error(format!(
            "Graph Relationship Type {key} DROP definition does not exactly match the current definition"
        )));
    }
    state.graph_relationships.remove(&key);
    detach_origin_constraints(state, &format!("graph/relationship/{key}"));
    Ok(())
}

fn remove_origin_dependent_constraints(state: &mut SchemaState, origin: &str) {
    state.constraints.retain(|_, constraint| {
        constraint.origin.as_deref() != Some(origin) || is_key_or_unique(constraint)
    });
}

fn detach_origin_constraints(state: &mut SchemaState, origin: &str) {
    state.constraints.retain(|_, constraint| {
        constraint.origin.as_deref() != Some(origin) || is_key_or_unique(constraint)
    });
    for constraint in state.constraints.values_mut() {
        if constraint.origin.as_deref() == Some(origin) {
            constraint.origin = None;
        }
    }
}

fn is_key_or_unique(constraint: &ConstraintDefinition) -> bool {
    matches!(
        constraint.kind,
        ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique
    )
}

fn insert_constraints(
    state: &mut SchemaState,
    constraints: Vec<ConstraintDefinition>,
    reject_existing: bool,
) -> QueryResult<()> {
    for constraint in constraints {
        insert_constraint(state, constraint, reject_existing)?;
    }
    Ok(())
}

fn insert_constraint(
    state: &mut SchemaState,
    constraint: ConstraintDefinition,
    reject_existing: bool,
) -> QueryResult<()> {
    if reject_existing
        && (state.constraints.contains_key(&constraint.name)
            || find_equivalent_constraint_name(state, &constraint).is_some())
    {
        return Err(schema_error(format!(
            "Constraint {} already exists",
            constraint.name
        )));
    }
    if let Some(existing) = state.constraints.get(&constraint.name)
        && existing != &constraint
    {
        return Err(schema_error(format!(
            "Constraint name {} is already in use",
            constraint.name
        )));
    }
    state
        .constraints
        .insert(constraint.name.clone(), constraint);
    Ok(())
}

fn apply_create_constraint(
    state: &mut SchemaState,
    clause: &AstNode,
    source: &str,
) -> QueryResult<()> {
    let if_not_exists = has_modifier(clause, ExistenceModifierKind::IfNotExists);
    let constraint = parse_constraint_definition(clause, source, None)?;
    let equivalent = find_equivalent_constraint_name(state, &constraint);
    let name_conflict = state.constraints.get(&constraint.name);
    if if_not_exists && (equivalent.is_some() || name_conflict.is_some()) {
        return Ok(());
    }
    if let Some(existing) = name_conflict {
        return if existing == &constraint {
            Err(schema_error(format!(
                "Constraint {} already exists",
                constraint.name
            )))
        } else {
            Err(schema_error(format!(
                "Constraint name {} is already in use",
                constraint.name
            )))
        };
    }
    if equivalent.is_some() {
        return Err(schema_error("an equivalent Constraint already exists"));
    }
    if let Some(index) = state.indexes.get(&constraint.name)
        && index.owning_constraint.as_deref() != Some(constraint.name.as_str())
    {
        return Err(schema_error(format!(
            "Schema name {} is already used by an Index",
            constraint.name
        )));
    }
    if let Some(backing) = constraint.backing_index()
        && state
            .indexes
            .values()
            .any(|existing| same_index_schema(existing, &backing))
    {
        return Err(schema_error(
            "Constraint conflicts with an existing range Index on the same schema",
        ));
    }
    state
        .constraints
        .insert(constraint.name.clone(), constraint);
    Ok(())
}

fn apply_drop_constraint(state: &mut SchemaState, clause: &AstNode) -> QueryResult<()> {
    let name = named_descendant(clause, AstKind::ConstraintName)?;
    if !state.constraints.contains_key(&name) {
        if has_modifier(clause, ExistenceModifierKind::IfExists) {
            return Ok(());
        }
        return Err(schema_error(format!("Constraint {name} does not exist")));
    }
    drop_named_constraint(state, &name)
}

fn drop_named_constraint(state: &mut SchemaState, name: &str) -> QueryResult<()> {
    let constraint = state
        .constraints
        .get(name)
        .ok_or_else(|| schema_error(format!("Constraint {name} does not exist")))?;
    if constraint.origin.is_some() && !is_key_or_unique(constraint) {
        return Err(schema_error(format!(
            "Constraint {name} belongs to a Graph Type element and must be changed through that element"
        )));
    }
    state.constraints.remove(name);
    Ok(())
}

fn parse_constraint_definition(
    node: &AstNode,
    _source: &str,
    origin: Option<String>,
) -> QueryResult<ConstraintDefinition> {
    let target = parse_schema_target(node)?;
    let requirement = node
        .descendants()
        .find(|child| child.kind == AstKind::ConstraintRequirement)
        .ok_or_else(|| schema_error("Constraint is missing its REQUIRE expression"))?;
    let properties = property_keys(requirement);
    if properties.is_empty() {
        return Err(schema_error("Constraint REQUIRE has no property"));
    }
    let kind = if let Some(type_expression) = requirement
        .descendants()
        .find(|child| child.kind == AstKind::TypeExpression)
    {
        if properties.len() != 1 {
            return Err(schema_error(
                "property type Constraints require exactly one property",
            ));
        }
        ConstraintDefinitionKind::Type {
            rule: parse_property_rule(type_expression)?,
        }
    } else {
        let kind = requirement
            .descendants()
            .find_map(constraint_kind)
            .ok_or_else(|| schema_error("Constraint is missing its kind"))?;
        map_constraint_kind(kind, &target)?
    };
    if matches!(kind, ConstraintDefinitionKind::NotNull) && properties.len() != 1 {
        return Err(schema_error(
            "NOT NULL Constraints require exactly one property",
        ));
    }
    let explicit_name = node
        .descendants()
        .find(|child| child.kind == AstKind::ConstraintName)
        .and_then(|child| child.text.as_deref())
        .map(cypher::unescape_identifier);
    let name = explicit_name.unwrap_or_else(|| {
        automatic_name(
            "constraint",
            &format!("{target:?}|{properties:?}|{kind:?}|{origin:?}"),
        )
    });
    Ok(ConstraintDefinition {
        name,
        target,
        properties,
        kind,
        origin,
    })
}

fn parse_schema_target(node: &AstNode) -> QueryResult<SchemaTarget> {
    if let Some(relationship) = node
        .descendants()
        .find(|child| child.kind == AstKind::RelationshipPattern)
    {
        let relationship_type = relationship
            .descendants()
            .find(|child| child.kind == AstKind::RelationshipTypeName)
            .and_then(|child| child.text.as_deref())
            .map(cypher::unescape_identifier)
            .ok_or_else(|| {
                schema_error("Relationship Constraint requires one Relationship Type")
            })?;
        return Ok(SchemaTarget::Relationship { relationship_type });
    }
    let label = node
        .descendants()
        .find(|child| child.kind == AstKind::NodePattern)
        .and_then(|pattern| {
            pattern
                .descendants()
                .find(|child| child.kind == AstKind::LabelName)
        })
        .and_then(|child| child.text.as_deref())
        .map(cypher::unescape_identifier)
        .ok_or_else(|| schema_error("Node Constraint requires one Node Label"))?;
    Ok(SchemaTarget::Node { label })
}

fn map_constraint_kind(
    kind: ConstraintKind,
    target: &SchemaTarget,
) -> QueryResult<ConstraintDefinitionKind> {
    match kind {
        ConstraintKind::Key | ConstraintKind::NodeKey | ConstraintKind::RelationshipKey => {
            validate_constraint_target_kind(kind, target)?;
            Ok(ConstraintDefinitionKind::Key)
        }
        ConstraintKind::Unique
        | ConstraintKind::NodeUnique
        | ConstraintKind::RelationshipUnique => {
            validate_constraint_target_kind(kind, target)?;
            Ok(ConstraintDefinitionKind::Unique)
        }
        ConstraintKind::NotNull => Ok(ConstraintDefinitionKind::NotNull),
    }
}

fn validate_constraint_target_kind(kind: ConstraintKind, target: &SchemaTarget) -> QueryResult<()> {
    let mismatch = matches!(
        (kind, target),
        (
            ConstraintKind::NodeKey | ConstraintKind::NodeUnique,
            SchemaTarget::Relationship { .. }
        ) | (
            ConstraintKind::RelationshipKey | ConstraintKind::RelationshipUnique,
            SchemaTarget::Node { .. }
        )
    );
    if mismatch {
        Err(schema_error(
            "Constraint kind does not match its graph element target",
        ))
    } else {
        Ok(())
    }
}

fn apply_create_index(state: &mut SchemaState, clause: &AstNode) -> QueryResult<()> {
    let if_not_exists = has_modifier(clause, ExistenceModifierKind::IfNotExists);
    let kind = clause
        .descendants()
        .find_map(|node| match node.kind {
            AstKind::IndexKind(kind) => Some(kind),
            _ => None,
        })
        .unwrap_or(IndexKind::Range);
    let kind = match kind {
        IndexKind::Lookup => StandardIndexKind::Lookup,
        IndexKind::Range => StandardIndexKind::Range,
        IndexKind::Text => StandardIndexKind::Text,
        IndexKind::Point => StandardIndexKind::Point,
        IndexKind::FullText | IndexKind::Vector => {
            return Err(QueryError::semantic(
                "FULLTEXT and VECTOR indexes are owned by Phase 08",
            ));
        }
    };
    let target = parse_index_target(clause, kind)?;
    let explicit_name = clause
        .descendants()
        .find(|node| node.kind == AstKind::IndexName)
        .and_then(|node| node.text.as_deref())
        .map(cypher::unescape_identifier);
    let name =
        explicit_name.unwrap_or_else(|| automatic_name("index", &format!("{kind:?}|{target:?}")));
    let index = IndexDefinition {
        name: name.clone(),
        kind,
        target,
        owning_constraint: None,
    };
    let equivalent = state.indexes.values().find(|existing| {
        existing.owning_constraint.is_none() && same_index_schema(existing, &index)
    });
    if state.constraints.contains_key(&name) {
        return Err(schema_error(format!(
            "Schema name {name} is already used by a Constraint"
        )));
    }
    if if_not_exists && (state.indexes.contains_key(&name) || equivalent.is_some()) {
        return Ok(());
    }
    if let Some(existing) = state.indexes.get(&name) {
        return if existing == &index {
            Err(schema_error(format!("Index {name} already exists")))
        } else {
            Err(schema_error(format!("Index name {name} is already in use")))
        };
    }
    if state
        .indexes
        .values()
        .any(|existing| same_index_schema(existing, &index))
    {
        return Err(schema_error("an equivalent Index already exists"));
    }
    state.indexes.insert(name, index);
    Ok(())
}

fn apply_drop_index(state: &mut SchemaState, clause: &AstNode) -> QueryResult<()> {
    let name = named_descendant(clause, AstKind::IndexName)?;
    let Some(index) = state.indexes.get(&name) else {
        if has_modifier(clause, ExistenceModifierKind::IfExists) {
            return Ok(());
        }
        return Err(schema_error(format!("Index {name} does not exist")));
    };
    if index.owning_constraint.is_some() {
        return Err(schema_error(format!(
            "Index {name} is owned by a Constraint; drop the Constraint instead"
        )));
    }
    state.indexes.remove(&name);
    Ok(())
}

fn parse_index_target(clause: &AstNode, kind: StandardIndexKind) -> QueryResult<IndexTarget> {
    let relationship = clause
        .descendants()
        .find(|node| node.kind == AstKind::RelationshipPattern);
    if kind == StandardIndexKind::Lookup {
        return Ok(if relationship.is_some() {
            IndexTarget::RelationshipLookup
        } else {
            IndexTarget::NodeLookup
        });
    }
    let target = clause
        .descendants()
        .find(|node| node.kind == AstKind::IndexTarget)
        .ok_or_else(|| schema_error("Index is missing its ON target"))?;
    let properties = target
        .descendants()
        .filter(|node| node.kind == AstKind::PropertyKey)
        .filter_map(|node| node.text.as_deref())
        .map(cypher::unescape_identifier)
        .collect::<Vec<_>>();
    if properties.is_empty() {
        return Err(schema_error(
            "property Index requires at least one property",
        ));
    }
    if matches!(kind, StandardIndexKind::Text | StandardIndexKind::Point) && properties.len() != 1 {
        return Err(schema_error(
            "TEXT and POINT indexes require exactly one property",
        ));
    }
    if let Some(relationship) = relationship {
        let relationship_type = relationship
            .descendants()
            .find(|node| node.kind == AstKind::RelationshipTypeName)
            .and_then(|node| node.text.as_deref())
            .map(cypher::unescape_identifier)
            .ok_or_else(|| schema_error("Relationship Index requires one Relationship Type"))?;
        return Ok(IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        });
    }
    let label = clause
        .descendants()
        .find(|node| node.kind == AstKind::NodePattern)
        .and_then(|pattern| {
            pattern
                .descendants()
                .find(|node| node.kind == AstKind::LabelName)
        })
        .and_then(|node| node.text.as_deref())
        .map(cypher::unescape_identifier)
        .ok_or_else(|| schema_error("Node Index requires one Node Label"))?;
    Ok(IndexTarget::NodeProperties { label, properties })
}

fn refresh_backing_indexes(state: &mut SchemaState) -> QueryResult<()> {
    state
        .indexes
        .retain(|_, index| index.owning_constraint.is_none());
    let backing = state
        .constraints
        .values()
        .filter_map(ConstraintDefinition::backing_index)
        .collect::<Vec<_>>();
    for index in backing {
        if state.indexes.contains_key(&index.name) {
            return Err(schema_error(format!(
                "Constraint {} cannot own its backing Index because that name is used by an explicit Index",
                index.name
            )));
        }
        state.indexes.insert(index.name.clone(), index);
    }
    Ok(())
}

fn validate_schema_state(state: &SchemaState) -> QueryResult<()> {
    validate_graph_type_state(state)?;
    validate_constraint_state(state)?;
    validate_index_state(state)
}

fn validate_graph_type_state(state: &SchemaState) -> QueryResult<()> {
    for definition in state.graph_nodes.values() {
        for implied in &definition.implied_labels {
            if state.graph_nodes.contains_key(implied) {
                return Err(schema_error(format!(
                    "Node label {implied} cannot be both identifying and implied in the current Graph Type"
                )));
            }
        }
    }
    for definition in state.graph_relationships.values() {
        for (label, identifying, endpoint) in [
            (
                definition.source_label.as_deref(),
                definition.source_identifying,
                "source",
            ),
            (
                definition.target_label.as_deref(),
                definition.target_identifying,
                "target",
            ),
        ] {
            if !identifying {
                continue;
            }
            let Some(label) = label else {
                return Err(schema_error(format!(
                    "Graph Relationship Type {} has an identifying {endpoint} endpoint without a label",
                    definition.relationship_type
                )));
            };
            if !state.graph_nodes.contains_key(label) {
                return Err(schema_error(format!(
                    "Graph Relationship Type {} identifying {endpoint} endpoint requires Graph Node Type {label}",
                    definition.relationship_type
                )));
            }
        }
    }
    Ok(())
}

fn validate_constraint_state(state: &SchemaState) -> QueryResult<()> {
    let constraints = state.constraints.values().collect::<Vec<_>>();
    for (position, constraint) in constraints.iter().enumerate() {
        if let ConstraintDefinitionKind::Type { rule } = &constraint.kind
            && constraints[..position].iter().any(|existing| {
                existing.target == constraint.target
                    && existing.properties == constraint.properties
                    && matches!(
                        &existing.kind,
                        ConstraintDefinitionKind::Type { rule: existing_rule }
                            if existing_rule != rule
                    )
            })
        {
            return Err(schema_error(format!(
                "property type Constraint {} conflicts with another property type Constraint on the same schema",
                constraint.name
            )));
        }
        if constraint.origin.is_some()
            || !matches!(
                constraint.kind,
                ConstraintDefinitionKind::NotNull | ConstraintDefinitionKind::Type { .. }
            )
        {
            continue;
        }
        let targets_identifying_element = match &constraint.target {
            SchemaTarget::Node { label } => state.graph_nodes.contains_key(label),
            SchemaTarget::Relationship { relationship_type } => {
                state.graph_relationships.contains_key(relationship_type)
            }
        };
        if targets_identifying_element {
            return Err(schema_error(format!(
                "independent property existence/type Constraint {} cannot target an identifying Graph Type label or relationship type",
                constraint.name
            )));
        }
    }
    Ok(())
}

fn validate_index_state(state: &SchemaState) -> QueryResult<()> {
    let indexes = state.indexes.values().collect::<Vec<_>>();
    for (position, index) in indexes.iter().enumerate() {
        if indexes[..position]
            .iter()
            .any(|existing| same_index_schema(existing, index))
        {
            return Err(schema_error(format!(
                "Index {} duplicates another Index on the same schema",
                index.name
            )));
        }
        validate_index_definition(state, index)?;
    }
    Ok(())
}

fn validate_index_definition(state: &SchemaState, index: &IndexDefinition) -> QueryResult<()> {
    let name = &index.name;
    if let Some(owner) = &index.owning_constraint {
        if owner != name || !state.constraints.contains_key(owner) {
            return Err(schema_error(format!(
                "Index {name} has an invalid owning Constraint"
            )));
        }
    } else if state.constraints.contains_key(name) {
        return Err(schema_error(format!(
            "Schema name {name} is shared by an independent Index and Constraint"
        )));
    }
    match (&index.kind, &index.target) {
        (StandardIndexKind::Lookup, IndexTarget::NodeLookup | IndexTarget::RelationshipLookup) => {}
        (StandardIndexKind::Lookup, _) => {
            return Err(schema_error(format!(
                "Lookup Index {name} has a property target"
            )));
        }
        (_, IndexTarget::NodeLookup | IndexTarget::RelationshipLookup) => {
            return Err(schema_error(format!(
                "Property Index {name} has a lookup target"
            )));
        }
        (
            StandardIndexKind::Text | StandardIndexKind::Point,
            IndexTarget::NodeProperties { properties, .. }
            | IndexTarget::RelationshipProperties { properties, .. },
        ) if properties.len() != 1 => {
            return Err(schema_error(format!(
                "Index {name} requires exactly one property"
            )));
        }
        _ => {}
    }
    Ok(())
}

fn same_index_schema(left: &IndexDefinition, right: &IndexDefinition) -> bool {
    left.kind == right.kind && left.target == right.target
}

fn type_constraint(
    origin: &str,
    target: SchemaTarget,
    property: String,
    rule: PropertyRule,
) -> ConstraintDefinition {
    let name = automatic_name(
        "graph_constraint",
        &format!("{origin}|{target:?}|{property}|type|{rule:?}"),
    );
    ConstraintDefinition {
        name,
        target,
        properties: vec![property],
        kind: ConstraintDefinitionKind::Type { rule },
        origin: Some(origin.to_owned()),
    }
}

fn key_or_unique_constraint(
    origin: &str,
    target: SchemaTarget,
    properties: Vec<String>,
    kind: ConstraintKind,
) -> QueryResult<ConstraintDefinition> {
    let mapped = map_constraint_kind(kind, &target)?;
    if !matches!(
        mapped,
        ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique
    ) {
        return Err(schema_error(
            "Graph Type property constraints support KEY or UNIQUE",
        ));
    }
    let name = automatic_name(
        "graph_constraint",
        &format!("{origin}|{target:?}|{properties:?}|{mapped:?}"),
    );
    Ok(ConstraintDefinition {
        name,
        target,
        properties,
        kind: mapped,
        origin: Some(origin.to_owned()),
    })
}

fn find_equivalent_constraint_name(
    state: &SchemaState,
    constraint: &ConstraintDefinition,
) -> Option<String> {
    state.constraints.values().find_map(|existing| {
        (existing.target == constraint.target
            && existing.properties == constraint.properties
            && existing.kind == constraint.kind
            && existing.origin == constraint.origin)
            .then(|| existing.name.clone())
    })
}

fn has_modifier(node: &AstNode, modifier: ExistenceModifierKind) -> bool {
    node.descendants()
        .any(|child| child.kind == AstKind::ExistenceModifier(modifier))
}

fn constraint_kind(node: &AstNode) -> Option<ConstraintKind> {
    match node.kind {
        AstKind::ConstraintKind(kind) => Some(kind),
        _ => None,
    }
}

fn property_keys(node: &AstNode) -> Vec<String> {
    node.descendants()
        .filter(|child| child.kind == AstKind::PropertyKey)
        .filter_map(|child| child.text.as_deref())
        .map(cypher::unescape_identifier)
        .collect()
}

fn first_property_key(node: &AstNode) -> QueryResult<String> {
    node.descendants()
        .find(|child| child.kind == AstKind::PropertyKey)
        .and_then(|child| child.text.as_deref())
        .map(cypher::unescape_identifier)
        .ok_or_else(|| schema_error("Schema property is missing its property key"))
}

fn named_descendant(node: &AstNode, kind: AstKind) -> QueryResult<String> {
    node.descendants()
        .find(|child| child.kind == kind)
        .and_then(|child| child.text.as_deref())
        .map(cypher::unescape_identifier)
        .ok_or_else(|| schema_error("Schema command is missing its object name"))
}

fn automatic_name(prefix: &str, canonical: &str) -> String {
    let hash = blake3::hash(canonical.as_bytes()).to_hex();
    format!("{prefix}_{}", &hash.as_str()[..16])
}
