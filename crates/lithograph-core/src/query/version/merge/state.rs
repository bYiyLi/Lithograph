use std::collections::BTreeMap;

use crate::query::version::{VersionExecution as Connection, VersionState};

use crate::cypher::{self, Value};
use crate::storage::{self, OwnerKind, RelationshipRecord, SnapshotState};

use super::{parse_element, parse_positive, string_map};
use crate::query::resources::{MemoryReservation, checked_add, checked_mul, map_entry_bytes};
use crate::query::{QueryError, QueryErrorKind, QueryResult};

pub(crate) struct VersionSlots {
    pub(crate) values: BTreeMap<String, Value>,
    pub(crate) memory: MemoryReservation,
}

impl std::ops::Deref for VersionSlots {
    type Target = BTreeMap<String, Value>;

    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl std::ops::DerefMut for VersionSlots {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.values
    }
}

impl VersionSlots {
    pub(crate) fn into_parts(self) -> (BTreeMap<String, Value>, MemoryReservation) {
        (self.values, self.memory)
    }

    fn reserve_entry(&mut self, key_bytes: u64) -> QueryResult<()> {
        self.memory
            .grow(checked_add(map_entry_bytes::<Value>()?, key_bytes)?)
    }
}

pub(crate) fn logical_slots(
    connection: &Connection,
    state: &SnapshotState,
) -> QueryResult<VersionSlots> {
    let mut slots = VersionSlots {
        values: BTreeMap::new(),
        memory: connection.reserve(0)?,
    };
    append_graph_slots(connection, state, &mut slots)?;
    append_property_slots(connection, state, &mut slots)?;
    append_schema_slots(connection, state, &mut slots)?;
    Ok(slots)
}

fn append_graph_slots(
    connection: &Connection,
    state: &SnapshotState,
    slots: &mut VersionSlots,
) -> QueryResult<()> {
    for node in &state.nodes {
        connection.checkpoint()?;
        slots.reserve_entry(32)?;
        slots.insert(format!("node/{node}"), Value::Boolean(true));
    }
    for (node, label) in &state.labels {
        append_label_slot(connection, slots, *node, *label)?;
    }
    for relationship in state.relationships.values() {
        connection.checkpoint()?;
        slots.reserve_entry(64)?;
        let (value, memory) = relationship_value(connection, *relationship)?;
        slots.memory.merge(memory)?;
        slots.insert(format!("relationship/{}", relationship.id), value);
    }
    Ok(())
}

fn append_label_slot(
    connection: &Connection,
    slots: &mut VersionSlots,
    node: i64,
    label: i64,
) -> QueryResult<()> {
    connection.checkpoint()?;
    storage::with_label_name(connection, label, |name| {
        let name = name.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                format!("LabelId {label} is missing"),
            )
        })?;
        slots.reserve_entry(checked_add(name.len() as u64, 64)?)?;
        slots.insert(format!("node/{node}/label/{name}"), Value::Boolean(true));
        Ok::<_, QueryError>(())
    })
}

fn append_property_slots(
    connection: &Connection,
    state: &SnapshotState,
    slots: &mut VersionSlots,
) -> QueryResult<()> {
    for ((owner, owner_id, key_id), value) in &state.properties {
        connection.checkpoint()?;
        storage::with_property_key_name(connection, *key_id, |key| {
            let key = key.ok_or_else(|| {
                QueryError::new(
                    QueryErrorKind::Storage,
                    format!("PropertyKeyId {key_id} is missing"),
                )
            })?;
            slots.reserve_entry(checked_add(key.len() as u64, 96)?)?;
            let owner = match owner {
                OwnerKind::Node => format!("node/{owner_id}"),
                OwnerKind::Relationship => format!("relationship/{owner_id}"),
            };
            let (value, memory) = connection.property_value(value)?;
            slots.memory.merge(memory)?;
            slots.insert(format!("{owner}/property/{key}"), value);
            Ok::<_, QueryError>(())
        })?;
    }
    Ok(())
}

fn append_schema_slots(
    connection: &Connection,
    state: &SnapshotState,
    slots: &mut VersionSlots,
) -> QueryResult<()> {
    for (name, definition) in &state.schema.graph_nodes {
        append_schema_slot(
            connection,
            slots,
            name,
            storage::graph_node_slot,
            definition,
        )?;
    }
    for (name, definition) in &state.schema.graph_relationships {
        append_schema_slot(
            connection,
            slots,
            name,
            storage::graph_relationship_slot,
            definition,
        )?;
    }
    for (name, definition) in &state.schema.constraints {
        append_schema_slot(
            connection,
            slots,
            name,
            storage::constraint_slot,
            definition,
        )?;
    }
    for (name, definition) in &state.schema.indexes {
        append_schema_slot(connection, slots, name, storage::index_slot, definition)?;
    }
    Ok(())
}

fn append_schema_slot<T: serde::Serialize>(
    connection: &Connection,
    slots: &mut VersionSlots,
    name: &str,
    slot: impl FnOnce(&str) -> String,
    definition: &T,
) -> QueryResult<()> {
    connection.checkpoint()?;
    slots.reserve_entry(checked_add(name.len() as u64, 32)?)?;
    let (value, memory) = serde_value(connection, definition)?;
    slots.memory.merge(memory)?;
    slots.insert(slot(name), value);
    Ok(())
}

pub(crate) fn set_logical_slot(
    connection: &Connection,
    state: &mut VersionState,
    slot: &str,
    value: Option<&Value>,
) -> QueryResult<()> {
    connection.checkpoint()?;
    if let Some(value) = value {
        let bytes = crate::query::resources::value_bytes_controlled(value, connection.control())?;
        let bytes = checked_add(checked_mul(bytes, 4)?, slot.len() as u64)?;
        state
            .memory
            .grow(checked_add(bytes, map_entry_bytes::<Value>()?)?)?;
    }
    if let Some(rest) = slot.strip_prefix("node/") {
        return set_node_slot(connection, state, rest, value);
    }
    if let Some(rest) = slot.strip_prefix("relationship/") {
        return set_relationship_slot(connection, state, rest, value);
    }
    set_schema_slot(connection, state, slot, value)
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
    connection: &Connection,
    state: &mut SnapshotState,
    slot: &str,
    value: Option<&Value>,
) -> QueryResult<()> {
    if let Some(name) = slot.strip_prefix("graph/node/") {
        return set_schema_value(connection, &mut state.schema.graph_nodes, name, value);
    }
    if let Some(name) = slot.strip_prefix("graph/relationship/") {
        return set_schema_value(
            connection,
            &mut state.schema.graph_relationships,
            name,
            value,
        );
    }
    if let Some(name) = slot.strip_prefix("constraint/") {
        return set_schema_value(connection, &mut state.schema.constraints, name, value);
    }
    if let Some(name) = slot.strip_prefix("index/") {
        return set_schema_value(connection, &mut state.schema.indexes, name, value);
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
        let owned = crate::query::expression::clone_value_unreserved_controlled(
            value,
            Some(connection.control()),
        )?;
        let property =
            crate::query::mutation::property_from_value_controlled(owned, connection.control())?
                .ok_or_else(|| {
                    QueryError::invalid_argument("property resolution cannot be null")
                })?;
        state.properties.insert(slot, property);
    } else {
        state.properties.remove(&slot);
    }
    Ok(())
}

fn set_schema_value<T>(
    connection: &Connection,
    values: &mut BTreeMap<String, T>,
    name: &str,
    value: Option<&Value>,
) -> QueryResult<()>
where
    T: serde::de::DeserializeOwned,
{
    if let Some(value) = value {
        let mut checkpoint = |work, bytes| {
            connection
                .control()
                .work(work)
                .and_then(|()| connection.control().bytes(bytes))
        };
        let json = cypher::encode_json_checked(value, &mut checkpoint)?;
        connection.control().check()?;
        let decoded = serde_json::from_value(json).map_err(|error| {
            QueryError::invalid_argument(format!("invalid schema resolution value: {error}"))
        })?;
        connection.control().check()?;
        values.insert(name.to_owned(), decoded);
    } else {
        values.remove(name);
    }
    Ok(())
}

pub(crate) fn serde_value<T: serde::Serialize>(
    connection: &Connection,
    value: &T,
) -> QueryResult<(Value, MemoryReservation)> {
    let control = connection.control();
    let mut count = JsonMeasure::new(control);
    let serialized = serde_json::to_writer(&mut count, value);
    if let Some(error) = count.error.take() {
        return Err(error);
    }
    serialized
        .map_err(|error| QueryError::internal(format!("failed to measure merge value: {error}")))?;
    let mut memory = connection.reserve(count.required()?)?;
    connection.checkpoint()?;
    let json = serde_json::to_value(value)
        .map_err(|error| QueryError::internal(format!("failed to encode merge value: {error}")))?;
    connection.checkpoint()?;
    let value = cypher::decode_json_checked(&json, &mut |units| {
        if units == 0 {
            control.check()
        } else {
            control.work(units as u64)
        }
    })?;
    drop(json);
    memory.resize(crate::query::resources::value_bytes_controlled(
        &value, control,
    )?)?;
    Ok((value, memory))
}

struct JsonMeasure<'a, 'b> {
    control: &'a crate::query::resources::ExecutionControl<'b>,
    bytes: u64,
    tokens: crate::query::version::metadata::JsonTokens,
    error: Option<QueryError>,
}

impl<'a, 'b> JsonMeasure<'a, 'b> {
    fn new(control: &'a crate::query::resources::ExecutionControl<'b>) -> Self {
        Self {
            control,
            bytes: 0,
            tokens: Default::default(),
            error: None,
        }
    }

    fn required(&self) -> QueryResult<u64> {
        self.tokens.required(self.bytes)
    }

    fn inspect(&mut self, bytes: &[u8]) -> QueryResult<()> {
        self.control.work(1)?;
        self.bytes = checked_add(self.bytes, bytes.len() as u64)?;
        for byte in bytes {
            self.control.bytes(1)?;
            self.tokens.inspect(*byte)?;
        }
        Ok(())
    }
}

impl std::io::Write for JsonMeasure<'_, '_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        match self.inspect(bytes) {
            Ok(()) => Ok(bytes.len()),
            Err(error) => {
                self.error = Some(error);
                Err(std::io::Error::other("merge serialization resource limit"))
            }
        }
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn relationship_value(
    connection: &Connection,
    relationship: RelationshipRecord,
) -> QueryResult<(Value, MemoryReservation)> {
    storage::with_relationship_type_name(connection, relationship.type_id, |name| {
        let name = name.ok_or_else(|| {
            QueryError::new(QueryErrorKind::Storage, "Relationship type is missing")
        })?;
        let bytes = checked_add(
            checked_mul(map_entry_bytes::<Value>()?, 3)?,
            checked_add(name.len() as u64, 128)?,
        )?;
        let memory = connection.reserve(bytes)?;
        let relationship_type =
            crate::query::expression::clone_string_controlled(name, Some(connection.control()))?;
        Ok((
            Value::Map(BTreeMap::from([
                ("type".to_owned(), Value::String(relationship_type)),
                (
                    "source".to_owned(),
                    Value::String(format!("n:{}", relationship.source)),
                ),
                (
                    "target".to_owned(),
                    Value::String(format!("n:{}", relationship.target)),
                ),
            ])),
            memory,
        ))
    })
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{ExecutionControl, QueryResources};

    #[test]
    fn schema_conversion_reserves_both_trees_before_building_and_releases() {
        let raw = rusqlite::Connection::open_in_memory().unwrap();
        let resources = QueryResources::new(Some(1024 * 1024));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let connection = Connection::new(&raw, &control);
        let definition = serde_json::json!({"properties": {"人物": ["名字", "描述"]}});
        let (value, memory) = serde_value(&connection, &definition).unwrap();
        assert_eq!(cypher::encode_json(&value), definition);
        assert!(resources.peak() > resources.used());
        assert_eq!(resources.used(), memory.bytes());
        drop(value);
        drop(memory);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn schema_conversion_rejects_before_owned_tree_and_is_cancelable() {
        let raw = rusqlite::Connection::open_in_memory().unwrap();
        let resources = QueryResources::new(Some(1024));
        let interrupted = || false;
        let control = ExecutionControl::new(&resources, &interrupted);
        let connection = Connection::new(&raw, &control);
        let text = "description".repeat(10_000);
        assert_eq!(
            serde_value(&connection, &text).unwrap_err().kind,
            QueryErrorKind::Resource
        );
        assert_eq!(resources.used(), 0);
        assert_eq!(resources.peak(), 0);

        let interrupted = || true;
        let control = ExecutionControl::new(&resources, &interrupted);
        let connection = Connection::new(&raw, &control);
        assert_eq!(
            serde_value(&connection, &text).unwrap_err().kind,
            QueryErrorKind::Interrupted
        );
        assert_eq!(resources.used(), 0);
    }
}
