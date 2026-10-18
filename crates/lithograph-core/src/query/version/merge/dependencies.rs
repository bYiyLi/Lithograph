//! Cancellable dependency checks without copied logical-slot key sets.

use std::collections::BTreeMap;

use super::{
    Connection, QueryResult, Value, VirtualValue, relationship_existence_slot,
    relationship_value_references_endpoint,
};
use crate::query::resources::checked_add;
use crate::query::version::canonical::same_optional_value_controlled;

pub(super) fn node_dependents_changed(
    connection: &Connection,
    base: &BTreeMap<String, VirtualValue>,
    side: &BTreeMap<String, Value>,
    node_id: &str,
) -> QueryResult<bool> {
    if node_children_changed(connection, base, side, node_id)? {
        return Ok(true);
    }
    relationship_dependencies_changed(connection, base, side, node_id)
}

fn node_children_changed(
    connection: &Connection,
    base: &BTreeMap<String, VirtualValue>,
    side: &BTreeMap<String, Value>,
    node_id: &str,
) -> QueryResult<bool> {
    let _prefix_memory = connection.reserve(checked_add(node_id.len() as u64, 32)?)?;
    let prefix = format!("node/{node_id}/");
    child_slots_changed(connection, base, side, &prefix)
}

fn relationship_dependencies_changed(
    connection: &Connection,
    base: &BTreeMap<String, VirtualValue>,
    side: &BTreeMap<String, Value>,
    node_id: &str,
) -> QueryResult<bool> {
    let _endpoint_memory = connection.reserve(checked_add(node_id.len() as u64, 16)?)?;
    let endpoint = format!("n:{node_id}");
    for (slot, value) in side {
        connection.checkpoint()?;
        if relationship_existence_slot(slot).is_none()
            || !relationship_value_references_endpoint(value, &endpoint)
        {
            continue;
        }
        if slot_changed(connection, base.get(slot), Some(value))? {
            return Ok(true);
        }
        let _prefix_memory = connection.reserve(checked_add(slot.len() as u64, 32)?)?;
        let prefix = format!("{slot}/property/");
        if child_slots_changed(connection, base, side, &prefix)? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn child_slots_changed(
    connection: &Connection,
    base: &BTreeMap<String, VirtualValue>,
    side: &BTreeMap<String, Value>,
    prefix: &str,
) -> QueryResult<bool> {
    for slot in base.keys().chain(side.keys()) {
        connection.checkpoint()?;
        if slot.starts_with(prefix) && slot_changed(connection, base.get(slot), side.get(slot))? {
            return Ok(true);
        }
    }
    Ok(false)
}

fn slot_changed(
    connection: &Connection,
    base: Option<&VirtualValue>,
    side: Option<&Value>,
) -> QueryResult<bool> {
    match base {
        Some(VirtualValue::Known(base)) => Ok(!same_optional_value_controlled(
            connection,
            base.as_ref(),
            side,
        )?),
        Some(VirtualValue::Unknown) => Ok(true),
        None => Ok(side.is_some()),
    }
}
