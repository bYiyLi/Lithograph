use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::cypher::{self, Value};
use crate::storage::{self, OwnerKind, RelationshipRecord, SnapshotState};

use super::{parse_element, parse_positive, string_map};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

pub(crate) fn logical_slots(
    connection: &Connection,
    state: &SnapshotState,
) -> QueryResult<BTreeMap<String, Value>> {
    let mut slots = BTreeMap::new();
    append_graph_slots(connection, state, &mut slots)?;
    append_property_slots(connection, state, &mut slots)?;
    append_schema_slots(state, &mut slots)?;
    Ok(slots)
}

fn append_graph_slots(
    connection: &Connection,
    state: &SnapshotState,
    slots: &mut BTreeMap<String, Value>,
) -> QueryResult<()> {
    for node in &state.nodes {
        slots.insert(format!("node/{node}"), Value::Boolean(true));
    }
    for (node, label) in &state.labels {
        let name = storage::label_name(connection, *label)?.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                format!("LabelId {label} is missing"),
            )
        })?;
        slots.insert(format!("node/{node}/label/{name}"), Value::Boolean(true));
    }
    for relationship in state.relationships.values() {
        slots.insert(
            format!("relationship/{}", relationship.id),
            relationship_value(connection, *relationship)?,
        );
    }
    Ok(())
}

fn append_property_slots(
    connection: &Connection,
    state: &SnapshotState,
    slots: &mut BTreeMap<String, Value>,
) -> QueryResult<()> {
    for ((owner, owner_id, key_id), value) in &state.properties {
        let key = storage::property_key_name(connection, *key_id)?.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                format!("PropertyKeyId {key_id} is missing"),
            )
        })?;
        let owner = match owner {
            OwnerKind::Node => format!("node/{owner_id}"),
            OwnerKind::Relationship => format!("relationship/{owner_id}"),
        };
        slots.insert(
            format!("{owner}/property/{key}"),
            crate::query::graph::property_value(value.clone())?,
        );
    }
    Ok(())
}

fn append_schema_slots(
    state: &SnapshotState,
    slots: &mut BTreeMap<String, Value>,
) -> QueryResult<()> {
    for (name, definition) in &state.schema.graph_nodes {
        slots.insert(storage::graph_node_slot(name), serde_value(definition)?);
    }
    for (name, definition) in &state.schema.graph_relationships {
        slots.insert(
            storage::graph_relationship_slot(name),
            serde_value(definition)?,
        );
    }
    for (name, definition) in &state.schema.constraints {
        slots.insert(storage::constraint_slot(name), serde_value(definition)?);
    }
    for (name, definition) in &state.schema.indexes {
        slots.insert(storage::index_slot(name), serde_value(definition)?);
    }
    Ok(())
}

pub(crate) fn set_logical_slot(
    connection: &Connection,
    state: &mut SnapshotState,
    slot: &str,
    value: Option<&Value>,
) -> QueryResult<()> {
    if let Some(rest) = slot.strip_prefix("node/") {
        return set_node_slot(connection, state, rest, value);
    }
    if let Some(rest) = slot.strip_prefix("relationship/") {
        return set_relationship_slot(connection, state, rest, value);
    }
    set_schema_slot(state, slot, value)
}

fn set_node_slot(
    connection: &Connection,
    state: &mut SnapshotState,
    rest: &str,
    value: Option<&Value>,
) -> QueryResult<()> {
    if let Some((id, label)) = rest.split_once("/label/") {
        let node = parse_positive(id, "NodeId")?;
        let label_id = storage::find_label(connection, label)?.ok_or_else(|| {
            QueryError::invalid_argument("resolution references an unknown Label")
        })?;
        if value.is_some() {
            state.labels.insert((node, label_id));
        } else {
            state.labels.remove(&(node, label_id));
        }
        return Ok(());
    }
    if let Some((id, key)) = rest.split_once("/property/") {
        return set_property_slot(
            connection,
            state,
            OwnerKind::Node,
            parse_positive(id, "NodeId")?,
            key,
            value,
        );
    }
    let node = parse_positive(rest, "NodeId")?;
    if value.is_some() {
        state.nodes.insert(node);
    } else {
        state.nodes.remove(&node);
    }
    Ok(())
}

fn set_relationship_slot(
    connection: &Connection,
    state: &mut SnapshotState,
    rest: &str,
    value: Option<&Value>,
) -> QueryResult<()> {
    if let Some((id, key)) = rest.split_once("/property/") {
        return set_property_slot(
            connection,
            state,
            OwnerKind::Relationship,
            parse_positive(id, "RelationshipId")?,
            key,
            value,
        );
    }
    let id = parse_positive(rest, "RelationshipId")?;
    if let Some(value) = value {
        state
            .relationships
            .insert(id, parse_relationship(connection, id, value)?);
    } else {
        state.relationships.remove(&id);
    }
    Ok(())
}

fn set_schema_slot(
    state: &mut SnapshotState,
    slot: &str,
    value: Option<&Value>,
) -> QueryResult<()> {
    if let Some(name) = slot.strip_prefix("graph/node/") {
        return set_schema_value(&mut state.schema.graph_nodes, name, value);
    }
    if let Some(name) = slot.strip_prefix("graph/relationship/") {
        return set_schema_value(&mut state.schema.graph_relationships, name, value);
    }
    if let Some(name) = slot.strip_prefix("constraint/") {
        return set_schema_value(&mut state.schema.constraints, name, value);
    }
    if let Some(name) = slot.strip_prefix("index/") {
        return set_schema_value(&mut state.schema.indexes, name, value);
    }
    Err(QueryError::invalid_argument(format!(
        "unsupported merge logical slot {slot:?}"
    )))
}

fn set_property_slot(
    connection: &Connection,
    state: &mut SnapshotState,
    owner: OwnerKind,
    owner_id: i64,
    key: &str,
    value: Option<&Value>,
) -> QueryResult<()> {
    let key_id = storage::find_property_key(connection, key)?.ok_or_else(|| {
        QueryError::invalid_argument("resolution references an unknown Property key")
    })?;
    let slot = (owner, owner_id, key_id);
    if let Some(value) = value {
        let property = crate::query::mutation::property_from_value(value.clone())?
            .ok_or_else(|| QueryError::invalid_argument("property resolution cannot be null"))?;
        state.properties.insert(slot, property);
    } else {
        state.properties.remove(&slot);
    }
    Ok(())
}

fn set_schema_value<T>(
    values: &mut BTreeMap<String, T>,
    name: &str,
    value: Option<&Value>,
) -> QueryResult<()>
where
    T: serde::de::DeserializeOwned,
{
    if let Some(value) = value {
        let decoded = serde_json::from_value(cypher::encode_json(value)).map_err(|error| {
            QueryError::invalid_argument(format!("invalid schema resolution value: {error}"))
        })?;
        values.insert(name.to_owned(), decoded);
    } else {
        values.remove(name);
    }
    Ok(())
}

fn serde_value<T: serde::Serialize>(value: &T) -> QueryResult<Value> {
    let json = serde_json::to_value(value)
        .map_err(|error| QueryError::internal(format!("failed to encode merge value: {error}")))?;
    cypher::decode_json(&json).map_err(Into::into)
}

fn relationship_value(
    connection: &Connection,
    relationship: RelationshipRecord,
) -> QueryResult<Value> {
    let relationship_type = storage::relationship_type_name(connection, relationship.type_id)?
        .ok_or_else(|| QueryError::new(QueryErrorKind::Storage, "Relationship type is missing"))?;
    Ok(Value::Map(BTreeMap::from([
        ("type".to_owned(), Value::String(relationship_type)),
        (
            "source".to_owned(),
            Value::String(format!("n:{}", relationship.source)),
        ),
        (
            "target".to_owned(),
            Value::String(format!("n:{}", relationship.target)),
        ),
    ])))
}

fn parse_relationship(
    connection: &Connection,
    id: i64,
    value: &Value,
) -> QueryResult<RelationshipRecord> {
    let Value::Map(value) = value else {
        return Err(QueryError::invalid_argument(
            "Relationship resolution must be a Map",
        ));
    };
    let relationship_type = string_map(value, "type")?;
    let type_id =
        storage::find_relationship_type(connection, relationship_type)?.ok_or_else(|| {
            QueryError::invalid_argument("resolution references an unknown Relationship type")
        })?;
    Ok(RelationshipRecord {
        id,
        source: parse_element(string_map(value, "source")?, "n:")?,
        target: parse_element(string_map(value, "target")?, "n:")?,
        type_id,
    })
}
