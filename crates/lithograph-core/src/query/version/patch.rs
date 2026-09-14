use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::Value;
use crate::storage::{
    self, CommitMetadata, HashId, IndexDefinition, OwnerKind, PropertyValue, RelationshipRecord,
    SchemaState, Snapshot,
};

use super::{
    ProcedureRow, commit_value, ensure_target_branch_head, map_target_branch_error, now_micros,
    resolve_descriptor_at, row, target_branch,
};
use crate::query::mutation::property_from_value;
use crate::query::options::ExecutionOptions;
use crate::query::{QueryError, QueryErrorKind, QueryResult};

pub(super) fn diff(connection: &Connection, args: Vec<Value>) -> QueryResult<Vec<ProcedureRow>> {
    let before_commit = resolve_descriptor_at(connection, &args, 0, "before")?;
    let after_commit = resolve_descriptor_at(connection, &args, 1, "after")?;
    let patch = diff_commits(connection, before_commit, after_commit)?;
    Ok(vec![row([("patch", patch)])])
}

pub(super) fn apply(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_head: HashId,
) -> QueryResult<Vec<ProcedureRow>> {
    let patch = args
        .first()
        .ok_or_else(|| QueryError::invalid_argument("missing patch"))?;
    let branch = target_branch(connection, options)?;
    ensure_target_branch_head(connection, &branch, pinned_head)?;
    let mut state = storage::load_snapshot_state(connection, pinned_head)?;
    apply_patch_to_state(connection, &mut state, patch)?;
    let base = storage::load_snapshot_state(connection, pinned_head)?;
    let layer = storage::layer_between(&base, &state)?;
    validate_candidate(connection, pinned_head, &layer, &state.schema)?;
    let schema_hash = state.schema.persist(connection)?;
    let metadata = CommitMetadata {
        author: options.author.clone(),
        message: options.message.clone(),
        committed_at: now_micros()?,
    };
    let commit = storage::commit_layer_with_schema(
        connection,
        &branch,
        pinned_head,
        None,
        &layer,
        schema_hash,
        &metadata,
    )
    .map_err(|error| map_target_branch_error(error, &branch))?;
    Ok(vec![row([("commit", commit_value(commit))])])
}

pub(crate) fn diff_commits(
    connection: &Connection,
    before_commit: HashId,
    after_commit: HashId,
) -> QueryResult<Value> {
    let before = storage::load_snapshot_state(connection, before_commit)?;
    let after = storage::load_snapshot_state(connection, after_commit)?;
    let operations = diff_states(connection, &before, &after)?;
    let database_id: String = connection
        .query_row(
            "SELECT database_id FROM main._lithograph_meta WHERE id = 1",
            [],
            |row| row.get(0),
        )
        .map_err(QueryError::from)?;
    Ok(Value::Map(BTreeMap::from([
        ("format".to_owned(), Value::Integer(1)),
        ("databaseId".to_owned(), Value::String(database_id)),
        ("from".to_owned(), commit_value(before_commit)),
        ("to".to_owned(), commit_value(after_commit)),
        ("operations".to_owned(), Value::List(operations)),
    ])))
}

pub(crate) fn diff_states(
    connection: &Connection,
    before: &storage::SnapshotState,
    after: &storage::SnapshotState,
) -> QueryResult<Vec<Value>> {
    let mut operations = Vec::new();
    append_node_and_label_operations(connection, before, after, &mut operations)?;
    append_relationship_operations(connection, before, after, &mut operations)?;
    append_property_operations(connection, before, after, &mut operations)?;
    append_schema_operations(&before.schema, &after.schema, &mut operations)?;
    operations.sort_by_key(canonical_operation_key);
    Ok(operations)
}

fn append_node_and_label_operations(
    connection: &Connection,
    before: &storage::SnapshotState,
    after: &storage::SnapshotState,
    operations: &mut Vec<Value>,
) -> QueryResult<()> {
    for node in before.nodes.difference(&after.nodes) {
        operations.push(operation(
            "DeleteNode",
            format!("node/{node}"),
            [
                ("elementId", Value::String(format!("n:{node}"))),
                ("before", Value::Boolean(true)),
                ("after", Value::Null),
            ],
        ));
    }
    for node in after.nodes.difference(&before.nodes) {
        operations.push(operation(
            "AddNode",
            format!("node/{node}"),
            [
                ("elementId", Value::String(format!("n:{node}"))),
                ("before", Value::Null),
                ("after", Value::Boolean(true)),
            ],
        ));
    }
    for (node, label) in before.labels.difference(&after.labels) {
        let label_name = label_name(connection, *label)?;
        operations.push(operation(
            "RemoveLabel",
            format!("node/{node}/label/{label_name}"),
            [
                ("elementId", Value::String(format!("n:{node}"))),
                ("label", Value::String(label_name)),
                ("before", Value::Boolean(true)),
                ("after", Value::Boolean(false)),
            ],
        ));
    }
    for (node, label) in after.labels.difference(&before.labels) {
        let label_name = label_name(connection, *label)?;
        operations.push(operation(
            "AddLabel",
            format!("node/{node}/label/{label_name}"),
            [
                ("elementId", Value::String(format!("n:{node}"))),
                ("label", Value::String(label_name)),
                ("before", Value::Boolean(false)),
                ("after", Value::Boolean(true)),
            ],
        ));
    }
    Ok(())
}

fn append_relationship_operations(
    connection: &Connection,
    before: &storage::SnapshotState,
    after: &storage::SnapshotState,
    operations: &mut Vec<Value>,
) -> QueryResult<()> {
    for (id, relationship) in &before.relationships {
        if !after.relationships.contains_key(id) {
            let value = relationship_value(connection, *relationship)?;
            operations.push(operation(
                "DeleteRelationship",
                format!("relationship/{id}"),
                [
                    ("elementId", Value::String(format!("r:{id}"))),
                    ("before", value),
                    ("after", Value::Null),
                ],
            ));
        }
    }
    for (id, relationship) in &after.relationships {
        match before.relationships.get(id) {
            None => {
                let value = relationship_value(connection, *relationship)?;
                operations.push(operation(
                    "AddRelationship",
                    format!("relationship/{id}"),
                    [
                        ("elementId", Value::String(format!("r:{id}"))),
                        ("before", Value::Null),
                        ("after", value),
                    ],
                ));
            }
            Some(previous) if previous != relationship => {
                return Err(QueryError::new(
                    QueryErrorKind::Storage,
                    format!("RelationshipId {id} changed immutable identity fields"),
                ));
            }
            Some(_) => {}
        }
    }
    Ok(())
}

fn append_property_operations(
    connection: &Connection,
    before: &storage::SnapshotState,
    after: &storage::SnapshotState,
    operations: &mut Vec<Value>,
) -> QueryResult<()> {
    let mut property_slots = BTreeSet::new();
    property_slots.extend(before.properties.keys().copied());
    property_slots.extend(after.properties.keys().copied());
    for slot in property_slots {
        let previous = before.properties.get(&slot);
        let next = after.properties.get(&slot);
        if previous == next {
            continue;
        }
        let key = storage::property_key_name(connection, slot.2)?.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                format!("PropertyKeyId {} is missing", slot.2),
            )
        })?;
        let owner = match slot.0 {
            OwnerKind::Node => format!("node/{}", slot.1),
            OwnerKind::Relationship => format!("relationship/{}", slot.1),
        };
        let mut extra = BTreeMap::from([
            ("owner".to_owned(), Value::String(owner.clone())),
            ("key".to_owned(), Value::String(key.clone())),
            ("before".to_owned(), property_state(previous)?),
            ("after".to_owned(), property_state(next)?),
        ]);
        operations.push(operation_map(
            if next.is_some() {
                "SetProperty"
            } else {
                "RemoveProperty"
            },
            format!("{owner}/property/{key}"),
            &mut extra,
        ));
    }
    Ok(())
}

pub(crate) fn apply_patch_to_state(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    patch: &Value,
) -> QueryResult<()> {
    let map = require_map(patch, "patch")?;
    if map.get("format") != Some(&Value::Integer(1)) {
        return Err(QueryError::invalid_argument("patch.format must be 1"));
    }
    let database_id = require_string(map.get("databaseId"), "patch.databaseId")?;
    let actual_database_id: String = connection.query_row(
        "SELECT database_id FROM main._lithograph_meta WHERE id = 1",
        [],
        |row| row.get(0),
    )?;
    if database_id != actual_database_id {
        return Err(QueryError::invalid_argument(
            "patch.databaseId does not match the current database",
        ));
    }
    validate_patch_commit_descriptor(map.get("from"), "patch.from")?;
    validate_patch_commit_descriptor(map.get("to"), "patch.to")?;
    let operations = match map.get("operations") {
        Some(Value::List(values)) => values,
        _ => {
            return Err(QueryError::invalid_argument(
                "patch.operations must be a List",
            ));
        }
    };
    validate_unique_operation_slots(operations)?;
    for value in operations {
        apply_operation(connection, state, value)?;
    }
    Ok(())
}

fn validate_unique_operation_slots(operations: &[Value]) -> QueryResult<()> {
    let mut slots = BTreeSet::new();
    for value in operations {
        let operation = require_map(value, "patch operation")?;
        let slot = require_string(operation.get("slot"), "operation.slot")?;
        if !slots.insert(slot) {
            return Err(QueryError::invalid_argument(format!(
                "patch contains duplicate logical slot {slot}"
            )));
        }
    }
    Ok(())
}

fn apply_operation(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    value: &Value,
) -> QueryResult<()> {
    let operation = require_map(value, "patch operation")?;
    let op = require_string(operation.get("op"), "operation.op")?;
    match op {
        "AddNode" | "DeleteNode" => apply_node_operation(connection, state, operation, op)?,
        "AddLabel" | "RemoveLabel" => apply_label_operation(connection, state, operation, op)?,
        "AddRelationship" | "DeleteRelationship" => {
            apply_relationship_operation(connection, state, operation, op)?;
        }
        "SetProperty" | "RemoveProperty" => {
            apply_property_operation(connection, state, operation, op)?
        }
        "SetSchema" => apply_schema_operation(state, operation)?,
        "CreateIndex" | "DropIndex" | "SetIndex" => apply_index_operation(state, operation, op)?,
        _ => {
            return Err(QueryError::invalid_argument(format!(
                "unknown patch operation {op}"
            )));
        }
    }
    Ok(())
}

fn apply_node_operation(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    op: &str,
) -> QueryResult<()> {
    let id = node_id(operation)?;
    require_operation_slot(operation, &format!("node/{id}"))?;
    let (expected_before, expected_after) = if op == "AddNode" {
        (Value::Null, Value::Boolean(true))
    } else {
        (Value::Boolean(true), Value::Null)
    };
    require_operation_state(operation, &expected_before, &expected_after)?;
    let matches_before = if op == "AddNode" {
        !state.nodes.contains(&id)
    } else {
        state.nodes.contains(&id)
    };
    if !matches_before {
        return before_mismatch(operation);
    }
    if op == "AddNode" {
        if !storage::node_id_is_allocated(connection, id)? {
            return Err(QueryError::invalid_argument(format!(
                "patch NodeId {id} was not allocated by this database"
            )));
        }
        state.nodes.insert(id);
    } else {
        state.nodes.remove(&id);
    }
    Ok(())
}

fn apply_label_operation(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    op: &str,
) -> QueryResult<()> {
    let id = node_id(operation)?;
    let label = require_string(operation.get("label"), "operation.label")?;
    require_operation_slot(operation, &format!("node/{id}/label/{label}"))?;
    let expected_before = Value::Boolean(op == "RemoveLabel");
    let expected_after = Value::Boolean(op == "AddLabel");
    if operation.get("before") != Some(&expected_before)
        || operation.get("after") != Some(&expected_after)
    {
        return Err(QueryError::invalid_argument(
            "Label operation before/after state does not match op",
        ));
    }
    let label_id = if op == "AddLabel" {
        storage::intern_label(connection, label)?
    } else {
        storage::find_label(connection, label)?.ok_or_else(|| {
            QueryError::invalid_argument("patch label before-condition does not match")
        })?
    };
    let present = state.labels.contains(&(id, label_id));
    if (op == "AddLabel") == present {
        return before_mismatch(operation);
    }
    if op == "AddLabel" {
        state.labels.insert((id, label_id));
    } else {
        state.labels.remove(&(id, label_id));
    }
    Ok(())
}

fn apply_relationship_operation(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    op: &str,
) -> QueryResult<()> {
    let id = relationship_id(operation)?;
    require_operation_slot(operation, &format!("relationship/{id}"))?;
    if op == "AddRelationship" {
        return apply_add_relationship(connection, state, operation, id);
    }
    apply_delete_relationship(connection, state, operation, id)
}

fn apply_add_relationship(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    id: i64,
) -> QueryResult<()> {
    if operation.get("before") != Some(&Value::Null) {
        return Err(QueryError::invalid_argument(
            "AddRelationship before must be null",
        ));
    }
    if state.relationships.contains_key(&id) {
        return before_mismatch(operation);
    }
    let record = relationship_record(connection, id, operation.get("after"))?;
    validate_allocated_relationship_identity(connection, id, record)?;
    state.relationships.insert(id, record);
    Ok(())
}

fn validate_allocated_relationship_identity(
    connection: &Connection,
    id: i64,
    record: RelationshipRecord,
) -> QueryResult<()> {
    let allocated = storage::relationship_id_is_allocated(connection, id)?
        && storage::node_id_is_allocated(connection, record.source)?
        && storage::node_id_is_allocated(connection, record.target)?;
    if allocated {
        return Ok(());
    }
    Err(QueryError::invalid_argument(
        "patch Relationship identity references an id that was not allocated by this database",
    ))
}

fn apply_delete_relationship(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    id: i64,
) -> QueryResult<()> {
    let expected = relationship_record(connection, id, operation.get("before"))?;
    if operation.get("after") != Some(&Value::Null) {
        return Err(QueryError::invalid_argument(
            "DeleteRelationship after must be null",
        ));
    }
    if state.relationships.get(&id) != Some(&expected) {
        return before_mismatch(operation);
    }
    state.relationships.remove(&id);
    Ok(())
}

fn apply_schema_operation(
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
) -> QueryResult<()> {
    require_operation_slot(operation, "schema")?;
    let current = schema_core_value(&state.schema)?;
    if operation.get("before") != Some(&current) {
        return before_mismatch(operation);
    }
    let after = operation
        .get("after")
        .ok_or_else(|| QueryError::invalid_argument("SetSchema is missing after"))?;
    apply_schema_core(&mut state.schema, after)
}

fn apply_index_operation(
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    op: &str,
) -> QueryResult<()> {
    let name = index_name_from_slot(operation)?;
    if op == "CreateIndex" {
        return create_index_from_patch(state, operation, name);
    }
    if op == "SetIndex" {
        return set_index_from_patch(state, operation, name);
    }
    drop_index_from_patch(state, operation, &name)
}

fn create_index_from_patch(
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    name: String,
) -> QueryResult<()> {
    if operation.get("before") != Some(&Value::Null) {
        return Err(QueryError::invalid_argument(
            "CreateIndex before must be null",
        ));
    }
    if state.schema.indexes.contains_key(&name) {
        return before_mismatch(operation);
    }
    let definition = index_definition(operation.get("after"))?;
    if definition.name != name {
        return Err(QueryError::invalid_argument(
            "CreateIndex slot/name mismatch",
        ));
    }
    state.schema.indexes.insert(name, definition);
    Ok(())
}

fn drop_index_from_patch(
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    name: &str,
) -> QueryResult<()> {
    let expected = index_definition(operation.get("before"))?;
    if expected.name != name {
        return Err(QueryError::invalid_argument("DropIndex slot/name mismatch"));
    }
    if operation.get("after") != Some(&Value::Null) {
        return Err(QueryError::invalid_argument("DropIndex after must be null"));
    }
    if state.schema.indexes.get(name) != Some(&expected) {
        return before_mismatch(operation);
    }
    state.schema.indexes.remove(name);
    Ok(())
}

fn set_index_from_patch(
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    name: String,
) -> QueryResult<()> {
    let expected = index_definition(operation.get("before"))?;
    let replacement = index_definition(operation.get("after"))?;
    if expected.name != name || replacement.name != name {
        return Err(QueryError::invalid_argument("SetIndex slot/name mismatch"));
    }
    if state.schema.indexes.get(&name) != Some(&expected) {
        return before_mismatch(operation);
    }
    state.schema.indexes.insert(name, replacement);
    Ok(())
}

fn apply_property_operation(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    op: &str,
) -> QueryResult<()> {
    let target = property_patch_target(connection, operation)?;
    validate_property_before(state, operation, &target)?;
    if op == "RemoveProperty" {
        if operation.get("after") != Some(&property_state(None)?) {
            return Err(QueryError::invalid_argument(
                "RemoveProperty after must be absent",
            ));
        }
        remove_property_target(state, &target);
        return Ok(());
    }
    set_property_target(connection, state, operation, &target)
}

fn validate_property_before(
    state: &storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    target: &PropertyPatchTarget,
) -> QueryResult<()> {
    let actual = target.existing_key.and_then(|key_id| {
        state
            .properties
            .get(&(target.owner_kind, target.owner_id, key_id))
    });
    if operation.get("before") != Some(&property_state(actual)?) {
        return before_mismatch(operation);
    }
    Ok(())
}

fn set_property_target(
    connection: &Connection,
    state: &mut storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    target: &PropertyPatchTarget,
) -> QueryResult<()> {
    let key_id = storage::intern_property_key(connection, &target.key)?;
    let value = patch_property_value(operation)?;
    state
        .properties
        .insert((target.owner_kind, target.owner_id, key_id), value);
    Ok(())
}

struct PropertyPatchTarget {
    owner_kind: OwnerKind,
    owner_id: i64,
    key: String,
    existing_key: Option<i64>,
}

fn property_patch_target(
    connection: &Connection,
    operation: &BTreeMap<String, Value>,
) -> QueryResult<PropertyPatchTarget> {
    let owner = require_string(operation.get("owner"), "operation.owner")?;
    let (owner_kind, owner_id) = parse_owner(owner)?;
    let key = require_string(operation.get("key"), "operation.key")?.to_owned();
    require_operation_slot(operation, &format!("{owner}/property/{key}"))?;
    let existing_key = storage::find_property_key(connection, &key)?;
    Ok(PropertyPatchTarget {
        owner_kind,
        owner_id,
        key,
        existing_key,
    })
}

fn remove_property_target(state: &mut storage::SnapshotState, target: &PropertyPatchTarget) {
    if let Some(key_id) = target.existing_key {
        state
            .properties
            .remove(&(target.owner_kind, target.owner_id, key_id));
    }
}

fn patch_property_value(operation: &BTreeMap<String, Value>) -> QueryResult<PropertyValue> {
    let after = require_map(
        operation
            .get("after")
            .ok_or_else(|| QueryError::invalid_argument("SetProperty is missing after"))?,
        "property state",
    )?;
    if after.get("present") != Some(&Value::Boolean(true)) {
        return Err(QueryError::invalid_argument(
            "SetProperty after must be present",
        ));
    }
    let value = after
        .get("value")
        .cloned()
        .ok_or_else(|| QueryError::invalid_argument("SetProperty after is missing value"))?;
    let value = property_from_value(value)?
        .ok_or_else(|| QueryError::invalid_argument("SetProperty cannot store null"))?;
    Ok(value)
}

pub(crate) fn validate_candidate(
    connection: &Connection,
    base_commit: HashId,
    layer: &storage::LayerBuilder,
    schema: &SchemaState,
) -> QueryResult<()> {
    let snapshot = Snapshot::resolve_with_layer(connection, base_commit, layer)?;
    snapshot.validate_graph_invariants()?;
    crate::query::schema::validate_snapshot(connection, schema, &snapshot)
}

fn append_schema_operations(
    before: &SchemaState,
    after: &SchemaState,
    operations: &mut Vec<Value>,
) -> QueryResult<()> {
    if schema_core_value(before)? != schema_core_value(after)? {
        operations.push(operation(
            "SetSchema",
            "schema".to_owned(),
            [
                ("before", schema_core_value(before)?),
                ("after", schema_core_value(after)?),
            ],
        ));
    }
    let mut names = BTreeSet::new();
    names.extend(before.indexes.keys().cloned());
    names.extend(after.indexes.keys().cloned());
    for name in names {
        let left = before.indexes.get(&name);
        let right = after.indexes.get(&name);
        if left == right {
            continue;
        }
        match (left, right) {
            (Some(left), Some(right)) => operations.push(operation(
                "SetIndex",
                format!("index/{name}"),
                [
                    ("before", index_value(left)?),
                    ("after", index_value(right)?),
                ],
            )),
            (Some(left), None) => operations.push(operation(
                "DropIndex",
                format!("index/{name}"),
                [("before", index_value(left)?), ("after", Value::Null)],
            )),
            (None, Some(right)) => operations.push(operation(
                "CreateIndex",
                format!("index/{name}"),
                [("before", Value::Null), ("after", index_value(right)?)],
            )),
            (None, None) => {}
        }
    }
    Ok(())
}

fn schema_core_value(schema: &SchemaState) -> QueryResult<Value> {
    let value = serde_json::json!({
        "graph_nodes": schema.graph_nodes,
        "graph_relationships": schema.graph_relationships,
        "constraints": schema.constraints,
    });
    super::json_to_value(value)
}

fn apply_schema_core(schema: &mut SchemaState, value: &Value) -> QueryResult<()> {
    let json = super::value_to_json(value)?;
    #[derive(serde::Deserialize)]
    struct Core {
        graph_nodes: BTreeMap<String, storage::GraphNodeType>,
        graph_relationships: BTreeMap<String, storage::GraphRelationshipType>,
        constraints: BTreeMap<String, storage::ConstraintDefinition>,
    }
    let core: Core = serde_json::from_value(json).map_err(|error| {
        QueryError::invalid_argument(format!("invalid SetSchema payload: {error}"))
    })?;
    schema.graph_nodes = core.graph_nodes;
    schema.graph_relationships = core.graph_relationships;
    schema.constraints = core.constraints;
    Ok(())
}

fn index_value(index: &IndexDefinition) -> QueryResult<Value> {
    super::json_to_value(serde_json::to_value(index).map_err(|error| {
        QueryError::internal(format!("failed to serialize Index definition: {error}"))
    })?)
}

fn index_definition(value: Option<&Value>) -> QueryResult<IndexDefinition> {
    let value = value
        .ok_or_else(|| QueryError::invalid_argument("Index operation is missing definition"))?;
    serde_json::from_value(super::value_to_json(value)?)
        .map_err(|error| QueryError::invalid_argument(format!("invalid Index definition: {error}")))
}

fn index_name_from_slot(operation: &BTreeMap<String, Value>) -> QueryResult<String> {
    let slot = require_string(operation.get("slot"), "operation.slot")?;
    slot.strip_prefix("index/")
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .ok_or_else(|| QueryError::invalid_argument("invalid Index operation slot"))
}

fn property_state(value: Option<&PropertyValue>) -> QueryResult<Value> {
    match value {
        None => Ok(Value::Map(BTreeMap::from([(
            "present".to_owned(),
            Value::Boolean(false),
        )]))),
        Some(value) => Ok(Value::Map(BTreeMap::from([
            ("present".to_owned(), Value::Boolean(true)),
            (
                "value".to_owned(),
                crate::query::graph::property_value(value.clone())?,
            ),
        ]))),
    }
}

fn relationship_value(connection: &Connection, record: RelationshipRecord) -> QueryResult<Value> {
    let relationship_type = storage::relationship_type_name(connection, record.type_id)?
        .ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                "Relationship type dictionary entry is missing",
            )
        })?;
    Ok(Value::Map(BTreeMap::from([
        ("type".to_owned(), Value::String(relationship_type)),
        (
            "source".to_owned(),
            Value::String(format!("n:{}", record.source)),
        ),
        (
            "target".to_owned(),
            Value::String(format!("n:{}", record.target)),
        ),
    ])))
}

fn relationship_record(
    connection: &Connection,
    id: i64,
    value: Option<&Value>,
) -> QueryResult<RelationshipRecord> {
    let map = require_map(
        value.ok_or_else(|| {
            QueryError::invalid_argument("Relationship operation is missing state")
        })?,
        "relationship state",
    )?;
    let relationship_type = require_string(map.get("type"), "relationship.type")?;
    let source = parse_element_id(
        require_string(map.get("source"), "relationship.source")?,
        "n:",
    )?;
    let target = parse_element_id(
        require_string(map.get("target"), "relationship.target")?,
        "n:",
    )?;
    let type_id = storage::intern_relationship_type(connection, relationship_type)?;
    Ok(RelationshipRecord {
        id,
        source,
        target,
        type_id,
    })
}

fn node_id(operation: &BTreeMap<String, Value>) -> QueryResult<i64> {
    parse_element_id(
        require_string(operation.get("elementId"), "operation.elementId")?,
        "n:",
    )
}

fn relationship_id(operation: &BTreeMap<String, Value>) -> QueryResult<i64> {
    parse_element_id(
        require_string(operation.get("elementId"), "operation.elementId")?,
        "r:",
    )
}

fn parse_element_id(value: &str, prefix: &str) -> QueryResult<i64> {
    value
        .strip_prefix(prefix)
        .and_then(|value| value.parse::<i64>().ok())
        .filter(|value| *value > 0)
        .ok_or_else(|| QueryError::invalid_argument(format!("invalid elementId {value:?}")))
}

fn parse_owner(value: &str) -> QueryResult<(OwnerKind, i64)> {
    if let Some(id) = value.strip_prefix("node/") {
        return id
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .map(|id| (OwnerKind::Node, id))
            .ok_or_else(|| QueryError::invalid_argument("invalid property owner"));
    }
    if let Some(id) = value.strip_prefix("relationship/") {
        return id
            .parse::<i64>()
            .ok()
            .filter(|id| *id > 0)
            .map(|id| (OwnerKind::Relationship, id))
            .ok_or_else(|| QueryError::invalid_argument("invalid property owner"));
    }
    Err(QueryError::invalid_argument("invalid property owner"))
}

fn label_name(connection: &Connection, label: i64) -> QueryResult<String> {
    storage::label_name(connection, label)?.ok_or_else(|| {
        QueryError::new(
            QueryErrorKind::Storage,
            format!("LabelId {label} is missing"),
        )
    })
}

fn operation<const N: usize>(op: &str, slot: String, extra: [(&str, Value); N]) -> Value {
    let mut map = BTreeMap::from([
        ("op".to_owned(), Value::String(op.to_owned())),
        ("slot".to_owned(), Value::String(slot)),
    ]);
    map.extend(
        extra
            .into_iter()
            .map(|(key, value)| (key.to_owned(), value)),
    );
    Value::Map(map)
}

fn operation_map(op: &str, slot: String, extra: &mut BTreeMap<String, Value>) -> Value {
    extra.insert("op".to_owned(), Value::String(op.to_owned()));
    extra.insert("slot".to_owned(), Value::String(slot));
    Value::Map(std::mem::take(extra))
}

fn canonical_operation_key(value: &Value) -> (String, u8, String) {
    let Value::Map(map) = value else {
        return (String::new(), 0, String::new());
    };
    let slot = match map.get("slot") {
        Some(Value::String(value)) => value.clone(),
        _ => String::new(),
    };
    let op = match map.get("op") {
        Some(Value::String(value)) => value.clone(),
        _ => String::new(),
    };
    let order = match op.as_str() {
        "DropIndex" => 0,
        "CreateIndex" => 1,
        _ => 0,
    };
    (slot, order, op)
}

fn require_map<'a>(value: &'a Value, role: &str) -> QueryResult<&'a BTreeMap<String, Value>> {
    match value {
        Value::Map(value) => Ok(value),
        _ => Err(QueryError::invalid_argument(format!(
            "{role} must be a Map"
        ))),
    }
}

fn require_string<'a>(value: Option<&'a Value>, role: &str) -> QueryResult<&'a str> {
    match value {
        Some(Value::String(value)) if !value.is_empty() => Ok(value),
        _ => Err(QueryError::invalid_argument(format!(
            "{role} must be a non-empty String"
        ))),
    }
}

fn validate_patch_commit_descriptor(value: Option<&Value>, role: &str) -> QueryResult<()> {
    let descriptor = require_string(value, role)?;
    let id = descriptor
        .strip_prefix("commit/")
        .filter(|id| !id.contains('/'))
        .ok_or_else(|| QueryError::invalid_argument(format!("{role} must use commit/<id>")))?;
    HashId::from_hex(id)
        .map(|_| ())
        .map_err(|_| QueryError::invalid_argument(format!("{role} has an invalid Commit id")))
}

fn require_operation_slot(operation: &BTreeMap<String, Value>, expected: &str) -> QueryResult<()> {
    if require_string(operation.get("slot"), "operation.slot")? != expected {
        return Err(QueryError::invalid_argument(
            "patch operation slot does not match its target",
        ));
    }
    Ok(())
}

fn require_operation_state(
    operation: &BTreeMap<String, Value>,
    expected_before: &Value,
    expected_after: &Value,
) -> QueryResult<()> {
    if operation.get("before") != Some(expected_before)
        || operation.get("after") != Some(expected_after)
    {
        return Err(QueryError::invalid_argument(
            "patch operation before/after state does not match op",
        ));
    }
    Ok(())
}

fn before_mismatch<T>(operation: &BTreeMap<String, Value>) -> QueryResult<T> {
    let slot = operation
        .get("slot")
        .and_then(|value| match value {
            Value::String(value) => Some(value.as_str()),
            _ => None,
        })
        .unwrap_or("<unknown>");
    Err(QueryError::invalid_argument(format!(
        "patch before-condition failed for {slot}"
    )))
}
