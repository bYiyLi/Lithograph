use std::collections::{BTreeMap, BTreeSet};

use super::{VersionExecution as Connection, VersionRows, VersionState, VersionValue};
use crate::query::resources::{
    MemoryReservation, allocation_bytes, checked_add, checked_mul, encoded_json_bytes_controlled,
    map_entry_bytes, value_bytes_controlled,
};

use crate::cypher::Value;
use crate::storage::{
    self, HashId, IndexDefinition, OwnerKind, PropertyValue, RelationshipRecord, SchemaState,
    Snapshot,
};

use super::canonical::{same_optional_value_controlled, same_value_controlled};
use super::{
    ProcedureRow, commit_value, ensure_target_branch_head, map_target_branch_error,
    resolve_descriptor_at, row, target_branch,
};
use crate::query::mutation::property_from_value_controlled;
use crate::query::options::ExecutionOptions;
use crate::query::{QueryError, QueryErrorKind, QueryResult};

#[path = "patch_resources.rs"]
mod resources;
pub(super) use resources::same_property_value_controlled;
use resources::*;

pub(super) fn diff(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
) -> QueryResult<VersionRows> {
    let before_commit = resolve_descriptor_at(connection, &args, 0, "before")?;
    let after_commit = resolve_descriptor_at(connection, &args, 1, "after")?;
    let patch = diff_commits(connection, before_commit, after_commit)?;
    super::access::check_patch_read(&patch, &options.access.read.query)?;
    let (patch, mut memory) = patch.into_parts();
    memory.grow(row_allocation_bytes(1)?)?;
    Ok(VersionRows {
        value: vec![row([("patch", patch)])],
        memory,
    })
}

pub(super) fn apply(
    connection: &Connection,
    args: Vec<Value>,
    options: &ExecutionOptions,
    pinned_head: HashId,
) -> QueryResult<VersionRows> {
    let patch = args
        .first()
        .ok_or_else(|| QueryError::invalid_argument("missing patch"))?;
    let branch = target_branch(connection, options)?;
    ensure_target_branch_head(connection, &branch, pinned_head)?;
    let mut state = connection.load_state(pinned_head)?;
    apply_patch_to_state(connection, &mut state, patch)?;
    let base = connection.load_state(pinned_head)?;
    let layer = connection.layer_between(&base, &state)?;
    validate_patch_access(
        connection,
        pinned_head,
        patch,
        &base,
        &state,
        &layer,
        &options.access,
    )?;
    validate_candidate(connection, pinned_head, &layer, &state.schema)?;
    let commit = commit_patch_candidate(connection, &branch, pinned_head, &state, &layer, options)?;
    let memory = connection.reserve(checked_add(row_allocation_bytes(1)?, 80)?)?;
    Ok(VersionRows {
        value: vec![row([("commit", commit_value(commit))])],
        memory,
    })
}

fn commit_patch_candidate(
    connection: &Connection,
    branch: &str,
    pinned_head: HashId,
    state: &storage::SnapshotState,
    layer: &storage::LayerBuilder,
    options: &ExecutionOptions,
) -> QueryResult<HashId> {
    let check = || {
        connection
            .control()
            .check()
            .map_err(super::execution::storage_error)
    };
    let _write_memory = connection.reserve(checked_mul(
        storage::snapshot_state_memory_bytes(state, &check)?,
        8,
    )?)?;
    let schema_blob = state.schema.canonical_blob_checked(&check)?;
    let schema_hash = storage::persist_schema_blob(connection, &schema_blob)?;
    let metadata = super::operations::copy_commit_metadata(connection, options)?;
    connection
        .commit_layer_with_schema(branch, pinned_head, None, layer, schema_hash, &metadata)
        .map_err(|error| map_target_branch_error(error, branch))
}

fn validate_patch_access(
    connection: &Connection,
    pinned_head: HashId,
    patch: &Value,
    base: &storage::SnapshotState,
    state: &storage::SnapshotState,
    layer: &storage::LayerBuilder,
    access: &crate::query::AccessPolicy,
) -> QueryResult<()> {
    if !access.write.properties.is_empty() {
        let after = connection.with_layer_snapshot(pinned_head, layer, &state.schema)?;
        super::access::check_patch_write(patch, &after, access)?;
    }
    super::access::check_layer(connection, pinned_head, layer, &state.schema, access)?;
    super::access::check_schema_validation(connection, &base.schema, &state.schema, access)
}

pub(crate) fn diff_commits(
    connection: &Connection,
    before_commit: HashId,
    after_commit: HashId,
) -> QueryResult<VersionValue> {
    let operations =
        if storage::is_first_parent_descendant(connection, before_commit, after_commit)? {
            diff_first_parent_commits(connection, before_commit, after_commit)?
        } else {
            let before = connection.load_state(before_commit)?;
            let after = connection.load_state(after_commit)?;
            diff_states(connection, &before, &after)?
        };
    patch_envelope(connection, before_commit, after_commit, operations)
}

fn diff_first_parent_commits(
    connection: &Connection,
    before_commit: HashId,
    after_commit: HashId,
) -> QueryResult<PatchOperations> {
    let layer = connection.layer_between_commits(before_commit, after_commit)?;
    let before_snapshot = connection.snapshot(before_commit)?;
    let mut operations = PatchOperations::new(connection)?;

    append_layer_operations(connection, &layer, &before_snapshot, &mut operations)?;

    let before_schema = connection.load_schema(before_commit)?;
    let after_schema = connection.load_schema(after_commit)?;
    append_schema_operations(connection, &before_schema, &after_schema, &mut operations)?;
    finish_operations(connection, operations)
}

fn append_layer_operations(
    connection: &Connection,
    layer: &storage::LayerBuilder,
    before_snapshot: &Snapshot<'_>,
    operations: &mut PatchOperations,
) -> QueryResult<()> {
    for (node_id, added) in layer.node_changes() {
        append_node_operation(connection, operations, node_id, added)?;
    }
    for (node_id, label_id, added) in layer.label_changes() {
        append_label_operation(connection, operations, node_id, label_id, added)?;
    }
    for (relationship, added) in layer.relationship_changes() {
        append_relationship_operation(connection, operations, relationship, added)?;
    }
    for (owner, owner_id, key_id, next) in layer.property_changes() {
        before_snapshot.with_property(owner, owner_id, key_id, |previous| {
            append_property_operation(
                connection,
                operations,
                (owner, owner_id, key_id),
                previous,
                next,
            )
        })?;
    }

    Ok(())
}

fn diff_states(
    connection: &Connection,
    before: &storage::SnapshotState,
    after: &storage::SnapshotState,
) -> QueryResult<PatchOperations> {
    let mut operations = PatchOperations::new(connection)?;
    append_node_and_label_operations(connection, before, after, &mut operations)?;
    append_relationship_operations(connection, before, after, &mut operations)?;
    append_property_operations(connection, before, after, &mut operations)?;
    append_schema_operations(connection, &before.schema, &after.schema, &mut operations)?;
    finish_operations(connection, operations)
}

fn finish_operations(
    connection: &Connection,
    mut operations: PatchOperations,
) -> QueryResult<PatchOperations> {
    connection.control().check()?;
    operations.sort_unstable_by(|left, right| {
        canonical_operation_key(left).cmp(&canonical_operation_key(right))
    });
    connection.control().check()?;
    Ok(operations)
}

fn append_node_operation(
    connection: &Connection,
    operations: &mut PatchOperations,
    node_id: i64,
    added: bool,
) -> QueryResult<()> {
    connection.checkpoint()?;
    operations.reserve_operation(5, 128)?;
    let (op, before, after) = if added {
        ("AddNode", Value::Null, Value::Boolean(true))
    } else {
        ("DeleteNode", Value::Boolean(true), Value::Null)
    };
    operations.push(operation(
        op,
        format!("node/{node_id}"),
        [
            ("elementId", Value::String(format!("n:{node_id}"))),
            ("before", before),
            ("after", after),
        ],
    ));
    Ok(())
}

fn append_label_operation(
    connection: &Connection,
    operations: &mut PatchOperations,
    node_id: i64,
    label_id: i64,
    added: bool,
) -> QueryResult<()> {
    connection.checkpoint()?;
    let (label, memory) = label_name(connection, label_id)?;
    operations.memory.merge(memory)?;
    operations.reserve_operation(6, checked_add(label.len() as u64, 128)?)?;
    operations.push(operation(
        if added { "AddLabel" } else { "RemoveLabel" },
        slot_text(connection, &format!("node/{node_id}/label/"), &label)?,
        [
            ("elementId", Value::String(format!("n:{node_id}"))),
            ("label", Value::String(label)),
            ("before", Value::Boolean(!added)),
            ("after", Value::Boolean(added)),
        ],
    ));
    Ok(())
}

fn append_relationship_operation(
    connection: &Connection,
    operations: &mut PatchOperations,
    relationship: RelationshipRecord,
    added: bool,
) -> QueryResult<()> {
    connection.checkpoint()?;
    operations.reserve_operation(5, 160)?;
    let (value, memory) = relationship_value(connection, relationship)?;
    operations.memory.merge(memory)?;
    let (before, after) = if added {
        (Value::Null, value)
    } else {
        (value, Value::Null)
    };
    operations.push(operation(
        if added {
            "AddRelationship"
        } else {
            "DeleteRelationship"
        },
        format!("relationship/{}", relationship.id),
        [
            ("elementId", Value::String(format!("r:{}", relationship.id))),
            ("before", before),
            ("after", after),
        ],
    ));
    Ok(())
}

fn append_property_operation(
    connection: &Connection,
    operations: &mut PatchOperations,
    slot: (OwnerKind, i64, i64),
    before: Option<&PropertyValue>,
    after: Option<&PropertyValue>,
) -> QueryResult<()> {
    connection.checkpoint()?;
    storage::with_property_key_name(connection, slot.2, |key| {
        let key = key.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                format!("PropertyKeyId {} is missing", slot.2),
            )
        })?;
        operations.reserve_operation(6, checked_add(checked_mul(key.len() as u64, 2)?, 192)?)?;
        let owner = match slot.0 {
            OwnerKind::Node => format!("node/{}", slot.1),
            OwnerKind::Relationship => format!("relationship/{}", slot.1),
        };
        let (before, before_memory) = property_state(connection, before)?;
        let (after_value, after_memory) = property_state(connection, after)?;
        operations.memory.merge(before_memory)?;
        operations.memory.merge(after_memory)?;
        let mut extra = BTreeMap::from([
            ("owner".to_owned(), Value::String(owner.clone())),
            ("key".to_owned(), Value::String(copy_text(connection, key)?)),
            ("before".to_owned(), before),
            ("after".to_owned(), after_value),
        ]);
        operations.push(operation_map(
            if after.is_some() {
                "SetProperty"
            } else {
                "RemoveProperty"
            },
            slot_text(connection, &format!("{owner}/property/"), key)?,
            &mut extra,
        ));
        Ok::<_, QueryError>(())
    })
}

fn append_node_and_label_operations(
    connection: &Connection,
    before: &storage::SnapshotState,
    after: &storage::SnapshotState,
    operations: &mut PatchOperations,
) -> QueryResult<()> {
    for node in before.nodes.difference(&after.nodes) {
        append_node_operation(connection, operations, *node, false)?;
    }
    for node in after.nodes.difference(&before.nodes) {
        append_node_operation(connection, operations, *node, true)?;
    }
    for (node, label) in before.labels.difference(&after.labels) {
        append_label_operation(connection, operations, *node, *label, false)?;
    }
    for (node, label) in after.labels.difference(&before.labels) {
        append_label_operation(connection, operations, *node, *label, true)?;
    }
    Ok(())
}

fn append_relationship_operations(
    connection: &Connection,
    before: &storage::SnapshotState,
    after: &storage::SnapshotState,
    operations: &mut PatchOperations,
) -> QueryResult<()> {
    for (id, relationship) in &before.relationships {
        if !after.relationships.contains_key(id) {
            append_relationship_operation(connection, operations, *relationship, false)?;
        }
    }
    for (id, relationship) in &after.relationships {
        match before.relationships.get(id) {
            None => append_relationship_operation(connection, operations, *relationship, true)?,
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
    operations: &mut PatchOperations,
) -> QueryResult<()> {
    for slot in before.properties.keys().chain(
        after
            .properties
            .keys()
            .filter(|slot| !before.properties.contains_key(slot)),
    ) {
        connection.checkpoint()?;
        let previous = before.properties.get(slot);
        let next = after.properties.get(slot);
        if same_property_value_controlled(connection, previous, next)? {
            continue;
        }
        append_property_operation(connection, operations, *slot, previous, next)?;
    }
    Ok(())
}

pub(crate) fn apply_patch_to_state(
    connection: &Connection,
    state: &mut VersionState,
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
    validate_unique_operation_slots(connection, operations)?;
    for value in operations {
        apply_operation(connection, state, value)?;
    }
    Ok(())
}

fn validate_unique_operation_slots(
    connection: &Connection,
    operations: &[Value],
) -> QueryResult<()> {
    let _memory = connection.reserve(checked_mul(
        operations.len() as u64,
        map_entry_bytes::<&str>()?,
    )?)?;
    let mut slots = BTreeSet::new();
    for value in operations {
        connection.checkpoint()?;
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
    state: &mut VersionState,
    value: &Value,
) -> QueryResult<()> {
    connection.checkpoint()?;
    let operation = require_map(value, "patch operation")?;
    reserve_patch_growth(connection, state, value)?;
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
        "SetSchema" => apply_schema_operation(connection, state, operation)?,
        "CreateIndex" | "DropIndex" | "SetIndex" => {
            apply_index_operation(connection, state, operation, op)?
        }
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
    state: &mut VersionState,
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
    state: &mut VersionState,
    operation: &BTreeMap<String, Value>,
    op: &str,
) -> QueryResult<()> {
    let id = node_id(operation)?;
    let label = require_string(operation.get("label"), "operation.label")?;
    require_operation_slot(
        operation,
        &slot_text(connection, &format!("node/{id}/label/"), label)?,
    )?;
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
    state: &mut VersionState,
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
    state: &mut VersionState,
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
    state: &mut VersionState,
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
    connection: &Connection,
    state: &mut VersionState,
    operation: &BTreeMap<String, Value>,
) -> QueryResult<()> {
    require_operation_slot(operation, "schema")?;
    let (current, _current_memory) = schema_core_value(connection, &state.schema)?;
    if !same_optional_value_controlled(connection, operation.get("before"), Some(&current))? {
        return before_mismatch(operation);
    }
    let after = operation
        .get("after")
        .ok_or_else(|| QueryError::invalid_argument("SetSchema is missing after"))?;
    apply_schema_core(connection, &mut state.schema, after)
}

fn apply_index_operation(
    connection: &Connection,
    state: &mut VersionState,
    operation: &BTreeMap<String, Value>,
    op: &str,
) -> QueryResult<()> {
    let name = index_name_from_slot(connection, operation)?;
    if op == "CreateIndex" {
        return create_index_from_patch(connection, state, operation, name);
    }
    if op == "SetIndex" {
        return set_index_from_patch(connection, state, operation, name);
    }
    drop_index_from_patch(connection, state, operation, &name)
}

fn create_index_from_patch(
    connection: &Connection,
    state: &mut VersionState,
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
    let definition = index_definition(connection, operation.get("after"))?;
    if definition.name != name {
        return Err(QueryError::invalid_argument(
            "CreateIndex slot/name mismatch",
        ));
    }
    state.schema.indexes.insert(name, definition);
    Ok(())
}

fn drop_index_from_patch(
    connection: &Connection,
    state: &mut VersionState,
    operation: &BTreeMap<String, Value>,
    name: &str,
) -> QueryResult<()> {
    let expected = index_definition(connection, operation.get("before"))?;
    if expected.name != name {
        return Err(QueryError::invalid_argument("DropIndex slot/name mismatch"));
    }
    if operation.get("after") != Some(&Value::Null) {
        return Err(QueryError::invalid_argument("DropIndex after must be null"));
    }
    if !same_index_controlled(connection, state.schema.indexes.get(name), Some(&expected))? {
        return before_mismatch(operation);
    }
    state.schema.indexes.remove(name);
    Ok(())
}

fn set_index_from_patch(
    connection: &Connection,
    state: &mut VersionState,
    operation: &BTreeMap<String, Value>,
    name: String,
) -> QueryResult<()> {
    let expected = index_definition(connection, operation.get("before"))?;
    let replacement = index_definition(connection, operation.get("after"))?;
    if expected.name != name || replacement.name != name {
        return Err(QueryError::invalid_argument("SetIndex slot/name mismatch"));
    }
    if !same_index_controlled(connection, state.schema.indexes.get(&name), Some(&expected))? {
        return before_mismatch(operation);
    }
    state.schema.indexes.insert(name, replacement);
    Ok(())
}

fn apply_property_operation(
    connection: &Connection,
    state: &mut VersionState,
    operation: &BTreeMap<String, Value>,
    op: &str,
) -> QueryResult<()> {
    let target = property_patch_target(connection, operation)?;
    validate_property_before(connection, state, operation, &target)?;
    if op == "RemoveProperty" {
        if operation.get("after") != Some(&property_state(connection, None)?.0) {
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
    connection: &Connection,
    state: &storage::SnapshotState,
    operation: &BTreeMap<String, Value>,
    target: &PropertyPatchTarget,
) -> QueryResult<()> {
    let actual = target.existing_key.and_then(|key_id| {
        state
            .properties
            .get(&(target.owner_kind, target.owner_id, key_id))
    });
    let (actual, _memory) = property_state(connection, actual)?;
    if !same_optional_value_controlled(connection, operation.get("before"), Some(&actual))? {
        return before_mismatch(operation);
    }
    Ok(())
}

fn set_property_target(
    connection: &Connection,
    state: &mut VersionState,
    operation: &BTreeMap<String, Value>,
    target: &PropertyPatchTarget,
) -> QueryResult<()> {
    let key_id = storage::intern_property_key(connection, &target.key)?;
    let value = patch_property_value(connection, operation)?;
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
    let key = copy_text(
        connection,
        require_string(operation.get("key"), "operation.key")?,
    )?;
    require_operation_slot(
        operation,
        &slot_text(connection, &format!("{owner}/property/"), &key)?,
    )?;
    let existing_key = storage::find_property_key(connection, &key)?;
    Ok(PropertyPatchTarget {
        owner_kind,
        owner_id,
        key,
        existing_key,
    })
}

fn remove_property_target(state: &mut VersionState, target: &PropertyPatchTarget) {
    if let Some(key_id) = target.existing_key {
        state
            .properties
            .remove(&(target.owner_kind, target.owner_id, key_id));
    }
}

fn patch_property_value(
    connection: &Connection,
    operation: &BTreeMap<String, Value>,
) -> QueryResult<PropertyValue> {
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
        .ok_or_else(|| QueryError::invalid_argument("SetProperty after is missing value"))?;
    let value = crate::query::expression::clone_value_unreserved_controlled(
        value,
        Some(connection.control()),
    )?;
    let value = property_from_value_controlled(value, connection.control())?
        .ok_or_else(|| QueryError::invalid_argument("SetProperty cannot store null"))?;
    Ok(value)
}

pub(crate) fn validate_candidate(
    connection: &Connection,
    base_commit: HashId,
    layer: &storage::LayerBuilder,
    schema: &SchemaState,
) -> QueryResult<()> {
    let snapshot = connection.with_layer_snapshot(base_commit, layer, schema)?;
    snapshot.validate_graph_invariants_checked(&super::execution::storage_check(
        connection.control(),
    ))?;
    crate::query::schema::validate_schema_transition_controlled(
        connection,
        &SchemaState::default(),
        schema,
        &snapshot,
        connection.control(),
    )?;
    let previous = connection.load_schema(base_commit)?;
    crate::query::semantic_index::validate_fulltext_transition_controlled(
        connection,
        &previous,
        schema,
        &|| connection.control().check().is_err(),
    )?;
    crate::query::managed_semantic::validate_transition_controlled(
        connection,
        &previous,
        schema,
        &|| connection.control().check().is_err(),
    )
}

fn append_schema_operations(
    connection: &Connection,
    before: &SchemaState,
    after: &SchemaState,
    operations: &mut PatchOperations,
) -> QueryResult<()> {
    let (left_core, left_memory) = schema_core_value(connection, before)?;
    let (right_core, right_memory) = schema_core_value(connection, after)?;
    if !same_value_controlled(connection, &left_core, &right_core)? {
        operations.reserve_operation(4, 64)?;
        operations.memory.merge(left_memory)?;
        operations.memory.merge(right_memory)?;
        operations.push(operation(
            "SetSchema",
            "schema".to_owned(),
            [("before", left_core), ("after", right_core)],
        ));
    }
    for name in before.indexes.keys().chain(
        after
            .indexes
            .keys()
            .filter(|name| !before.indexes.contains_key(*name)),
    ) {
        append_index_operation(
            connection,
            operations,
            name,
            before.indexes.get(name),
            after.indexes.get(name),
        )?;
    }
    Ok(())
}

fn index_name_from_slot(
    connection: &Connection,
    operation: &BTreeMap<String, Value>,
) -> QueryResult<String> {
    let slot = require_string(operation.get("slot"), "operation.slot")?;
    let name = slot
        .strip_prefix("index/")
        .filter(|value| !value.is_empty())
        .ok_or_else(|| QueryError::invalid_argument("invalid Index operation slot"))?;
    copy_text(connection, name)
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

fn canonical_operation_key(value: &Value) -> (&str, u8, &str) {
    let Value::Map(map) = value else {
        return ("", 0, "");
    };
    let slot = match map.get("slot") {
        Some(Value::String(value)) => value.as_str(),
        _ => "",
    };
    let op = match map.get("op") {
        Some(Value::String(value)) => value.as_str(),
        _ => "",
    };
    let order = match op {
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

fn append_index_operation(
    connection: &Connection,
    operations: &mut PatchOperations,
    name: &str,
    left: Option<&IndexDefinition>,
    right: Option<&IndexDefinition>,
) -> QueryResult<()> {
    connection.checkpoint()?;
    if same_index_controlled(connection, left, right)? {
        return Ok(());
    }
    operations.reserve_operation(4, checked_add(name.len() as u64, 64)?)?;
    let (op, left, right) = match (left, right) {
        (Some(left), Some(right)) => ("SetIndex", Some(left), Some(right)),
        (Some(left), None) => ("DropIndex", Some(left), None),
        (None, Some(right)) => ("CreateIndex", None, Some(right)),
        (None, None) => return Ok(()),
    };
    let before_value = match left {
        Some(value) => {
            let (value, memory) = index_value(connection, value)?;
            operations.memory.merge(memory)?;
            value
        }
        None => Value::Null,
    };
    let after_value = match right {
        Some(value) => {
            let (value, memory) = index_value(connection, value)?;
            operations.memory.merge(memory)?;
            value
        }
        None => Value::Null,
    };
    operations.push(operation(
        op,
        slot_text(connection, "index/", name)?,
        [("before", before_value), ("after", after_value)],
    ));
    Ok(())
}
