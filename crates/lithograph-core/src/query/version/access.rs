use std::collections::BTreeSet;

use rusqlite::Connection;

use crate::cypher::Value;
use crate::query::{AccessPolicy, PropertyRules, QueryError, QueryResult};
use crate::storage::{self, HashId, LayerBuilder, OwnerKind, SchemaState, Snapshot};

/// Checks a final effective graph delta, rather than the historical Layers used
/// to produce it. Deleted entities own their implicit property/Label cleanup.
pub(super) fn check_layer(
    connection: &Connection,
    before_commit: HashId,
    layer: &LayerBuilder,
    next_schema: &SchemaState,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    if unrestricted_graph_writes(policy) {
        return Ok(());
    }
    let before = Snapshot::resolve(connection, before_commit)?;
    let after = Snapshot::resolve_with_layer(connection, before_commit, layer)?;
    check_layer_entities(&after, layer, policy)?;
    check_touched_properties(connection, &before, &after, layer, policy)?;
    let previous = SchemaState::load(connection, before_commit)?;
    check_schema_transition(&previous, next_schema, policy)
}

fn check_layer_entities(
    after: &Snapshot<'_>,
    layer: &LayerBuilder,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    for (_, added) in layer.node_changes() {
        check_entity_change(added, policy)?;
    }
    for (_, added) in layer.relationship_changes() {
        check_entity_change(added, policy)?;
    }
    for (node, _, _) in layer.label_changes() {
        if after.node_exists(node)? {
            require(
                policy.write.allow_label_changes,
                "writeAccess.allowLabelChanges",
            )?;
        }
    }
    Ok(())
}

pub(super) fn check_schema_transition(
    previous: &SchemaState,
    next: &SchemaState,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    if previous.canonical_blob()? != next.canonical_blob()? {
        require(
            policy.write.allow_schema_changes,
            "writeAccess.allowSchemaChanges",
        )?;
    }
    Ok(())
}

pub(super) fn check_schema_validation(
    previous: &SchemaState,
    next: &SchemaState,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    crate::query::schema::validate_external_transition(previous, next, policy)
}

/// Arbitrary Branch moves inspect only slots touched on either first-parent
/// chain. No complete before/after graph is copied for access checking.
pub(super) fn check_commits(
    connection: &Connection,
    before_commit: HashId,
    after_commit: HashId,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    if before_commit == after_commit || unrestricted_graph_writes(policy) {
        return Ok(());
    }
    let common = first_parent_common(connection, before_commit, after_commit)?;
    let before_touched = storage::touched_layer_between_commits(connection, common, before_commit)?;
    let after_touched = storage::touched_layer_between_commits(connection, common, after_commit)?;
    let before = Snapshot::resolve(connection, before_commit)?;
    let after = Snapshot::resolve(connection, after_commit)?;
    let mut touched = before_touched;
    touched.nodes.extend(after_touched.nodes);
    touched.labels.extend(after_touched.labels);
    touched.relationships.extend(after_touched.relationships);
    touched.properties.extend(after_touched.properties);
    check_touched_entities(&before, &after, &touched, policy)?;
    check_touched_properties(connection, &before, &after, &touched, policy)?;
    let previous = SchemaState::load(connection, before_commit)?;
    let next = SchemaState::load(connection, after_commit)?;
    check_schema_transition(&previous, &next, policy)
}

fn unrestricted_graph_writes(policy: &AccessPolicy) -> bool {
    policy.write.properties.is_empty()
        && policy.write.allow_create
        && policy.write.allow_delete
        && policy.write.allow_label_changes
        && policy.write.allow_schema_changes
}

fn first_parent_common(
    connection: &Connection,
    mut left: HashId,
    mut right: HashId,
) -> QueryResult<HashId> {
    let mut ancestors = BTreeSet::new();
    loop {
        ancestors.insert(left);
        let Some(parent) = storage::load_commit(connection, left)?.parent1 else {
            break;
        };
        left = parent;
    }
    loop {
        if ancestors.contains(&right) {
            return Ok(right);
        }
        right = storage::load_commit(connection, right)?
            .parent1
            .ok_or_else(|| QueryError::internal("Version operations have no common Root Commit"))?;
    }
}

fn check_touched_entities(
    before: &Snapshot<'_>,
    after: &Snapshot<'_>,
    touched: &LayerBuilder,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    for (node, _) in touched.node_changes() {
        let was_present = before.node_exists(node)?;
        let is_present = after.node_exists(node)?;
        if was_present != is_present {
            check_entity_change(is_present, policy)?;
        }
    }
    for (relationship, _) in touched.relationship_changes() {
        let was_present = before.relationship(relationship.id)?.is_some();
        let is_present = after.relationship(relationship.id)?.is_some();
        if was_present != is_present {
            check_entity_change(is_present, policy)?;
        }
    }
    check_touched_labels(before, after, touched, policy)?;
    Ok(())
}

fn check_touched_labels(
    before: &Snapshot<'_>,
    after: &Snapshot<'_>,
    touched: &LayerBuilder,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    if policy.write.allow_label_changes {
        return Ok(());
    }
    for (node, _, _) in touched.label_changes() {
        if after.node_exists(node)? && before.labels(node)? != after.labels(node)? {
            return require(false, "writeAccess.allowLabelChanges");
        }
    }
    Ok(())
}

fn check_touched_properties(
    connection: &Connection,
    before: &Snapshot<'_>,
    after: &Snapshot<'_>,
    touched: &LayerBuilder,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    if policy.write.properties.is_empty() {
        return Ok(());
    }
    for (owner, id, key, _) in touched.property_changes() {
        if owner_exists(after, owner, id)? && !same_property_state(before, after, owner, id, key)? {
            policy.check_write_property(&property_name(connection, key)?)?;
        }
    }
    Ok(())
}

fn same_property_state(
    before: &Snapshot<'_>,
    after: &Snapshot<'_>,
    owner: OwnerKind,
    id: i64,
    key: i64,
) -> QueryResult<bool> {
    let previous = before
        .property(owner, id, key)?
        .as_ref()
        .map(storage::PropertyValue::canonical_bytes)
        .transpose()?;
    let next = after
        .property(owner, id, key)?
        .as_ref()
        .map(storage::PropertyValue::canonical_bytes)
        .transpose()?;
    Ok(previous == next)
}

fn owner_exists(snapshot: &Snapshot<'_>, owner: OwnerKind, id: i64) -> QueryResult<bool> {
    Ok(match owner {
        OwnerKind::Node => snapshot.node_exists(id)?,
        OwnerKind::Relationship => snapshot.relationship(id)?.is_some(),
    })
}

fn check_entity_change(added: bool, policy: &AccessPolicy) -> QueryResult<()> {
    if added {
        require(policy.write.allow_create, "writeAccess.allowCreate")
    } else {
        require(policy.write.allow_delete, "writeAccess.allowDelete")
    }
}

fn property_name(connection: &Connection, key: i64) -> QueryResult<String> {
    storage::property_key_name(connection, key)?
        .ok_or_else(|| QueryError::internal("Version property dictionary entry is missing"))
}

pub(super) fn check_slot_read(slot: &str, rules: &PropertyRules) -> QueryResult<()> {
    if graph_property_name(slot).is_some_and(|key| rules.denies(key)) {
        return Err(QueryError::access_denied(
            "readAccess.query forbids exporting complete version data containing a protected graph property",
        ));
    }
    Ok(())
}

pub(super) fn check_slot_write(slot: &str, policy: &AccessPolicy) -> QueryResult<()> {
    if let Some(key) = graph_property_name(slot) {
        policy.check_write_property(key)?;
    }
    Ok(())
}

pub(super) fn check_resolution_slot(
    slot: &str,
    previous: Option<&Value>,
    next: Option<&Value>,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    if graph_property_name(slot).is_some() {
        return check_slot_write(slot, policy);
    }
    if super::canonical::same_optional_value(previous, next) {
        return Ok(());
    }
    if slot.starts_with("node/") || slot.starts_with("relationship/") {
        if slot.contains("/label/") {
            require(
                policy.write.allow_label_changes,
                "writeAccess.allowLabelChanges",
            )?;
        } else if !slot.contains("/property/") {
            check_entity_change(next.is_some(), policy)?;
        }
    } else {
        require(
            policy.write.allow_schema_changes,
            "writeAccess.allowSchemaChanges",
        )?;
    }
    Ok(())
}

fn graph_property_name(slot: &str) -> Option<&str> {
    graph_property_owner(slot).map(|(_, _, key)| key)
}

fn graph_property_owner(slot: &str) -> Option<(OwnerKind, i64, &str)> {
    let (owner_kind, rest) = if let Some(rest) = slot.strip_prefix("node/") {
        (OwnerKind::Node, rest)
    } else {
        (OwnerKind::Relationship, slot.strip_prefix("relationship/")?)
    };
    let (owner, key) = rest.split_once("/property/")?;
    let id = owner.parse::<i64>().ok().filter(|id| *id > 0)?;
    Some((owner_kind, id, key))
}

pub(super) fn check_patch_read(patch: &Value, rules: &PropertyRules) -> QueryResult<()> {
    if rules.is_empty() {
        return Ok(());
    }
    for slot in patch_operation_slots(patch) {
        check_slot_read(slot, rules)?;
    }
    Ok(())
}

pub(super) fn check_patch_write(
    patch: &Value,
    after: &Snapshot<'_>,
    policy: &AccessPolicy,
) -> QueryResult<()> {
    if policy.write.properties.is_empty() {
        return Ok(());
    }
    for slot in patch_operation_slots(patch) {
        let Some((owner, id, _)) = graph_property_owner(slot) else {
            continue;
        };
        if owner_exists(after, owner, id)? {
            check_slot_write(slot, policy)?;
        }
    }
    Ok(())
}

fn patch_operation_slots(patch: &Value) -> impl Iterator<Item = &str> {
    patch_operations(patch).iter().filter_map(operation_slot)
}

fn patch_operations(patch: &Value) -> &[Value] {
    let Value::Map(patch) = patch else {
        return &[];
    };
    match patch.get("operations") {
        Some(Value::List(operations)) => operations,
        _ => &[],
    }
}

fn operation_slot(operation: &Value) -> Option<&str> {
    let Value::Map(operation) = operation else {
        return None;
    };
    match operation.get("slot") {
        Some(Value::String(slot)) => Some(slot),
        _ => None,
    }
}

fn require(allowed: bool, permission: &str) -> QueryResult<()> {
    crate::query::access::require(allowed, permission)
}
