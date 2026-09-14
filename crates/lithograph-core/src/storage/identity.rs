use rusqlite::{Connection, OptionalExtension, params};

use super::NodeId;
use super::{
    LabelId, PropertyKeyId, RelationshipId, RelationshipTypeId, StorageError, StorageResult,
};

const NODE_SEQUENCE: i64 = 1;
const RELATIONSHIP_SEQUENCE: i64 = 2;
const LABEL_SEQUENCE: i64 = 3;
const RELATIONSHIP_TYPE_SEQUENCE: i64 = 4;
const PROPERTY_KEY_SEQUENCE: i64 = 5;

/// Allocates the next database-wide NodeId.
pub fn allocate_node_id(connection: &Connection) -> StorageResult<NodeId> {
    allocate_id(connection, NODE_SEQUENCE)
}

/// Allocates the next database-wide RelationshipId.
pub fn allocate_relationship_id(connection: &Connection) -> StorageResult<RelationshipId> {
    allocate_id(connection, RELATIONSHIP_SEQUENCE)
}

/// Returns whether a NodeId has already been allocated by this database.
pub fn node_id_is_allocated(connection: &Connection, id: NodeId) -> StorageResult<bool> {
    id_is_allocated(connection, NODE_SEQUENCE, id)
}

/// Returns whether a RelationshipId has already been allocated by this database.
pub fn relationship_id_is_allocated(
    connection: &Connection,
    id: RelationshipId,
) -> StorageResult<bool> {
    id_is_allocated(connection, RELATIONSHIP_SEQUENCE, id)
}

/// Returns the append-only dictionary id for a label name.
pub fn intern_label(connection: &Connection, name: &str) -> StorageResult<LabelId> {
    intern_name(connection, "_lithograph_labels", LABEL_SEQUENCE, name)
}

/// Returns the append-only dictionary id for a relationship type name.
pub fn intern_relationship_type(
    connection: &Connection,
    name: &str,
) -> StorageResult<RelationshipTypeId> {
    intern_name(
        connection,
        "_lithograph_rel_types",
        RELATIONSHIP_TYPE_SEQUENCE,
        name,
    )
}

/// Returns the append-only dictionary id for a property-key name.
pub fn intern_property_key(connection: &Connection, name: &str) -> StorageResult<PropertyKeyId> {
    intern_name(
        connection,
        "_lithograph_prop_keys",
        PROPERTY_KEY_SEQUENCE,
        name,
    )
}

/// Looks up an existing Label dictionary id without allocating one.
pub fn find_label(connection: &Connection, name: &str) -> StorageResult<Option<LabelId>> {
    find_name(connection, "_lithograph_labels", name)
}

/// Looks up an existing Relationship Type dictionary id without allocating one.
pub fn find_relationship_type(
    connection: &Connection,
    name: &str,
) -> StorageResult<Option<RelationshipTypeId>> {
    find_name(connection, "_lithograph_rel_types", name)
}

/// Looks up an existing Property Key dictionary id without allocating one.
pub fn find_property_key(
    connection: &Connection,
    name: &str,
) -> StorageResult<Option<PropertyKeyId>> {
    find_name(connection, "_lithograph_prop_keys", name)
}

/// Resolves a Label dictionary id to its exact stored name.
pub fn label_name(connection: &Connection, id: LabelId) -> StorageResult<Option<String>> {
    name_for_id(connection, "_lithograph_labels", id)
}

/// Resolves a Relationship Type dictionary id to its exact stored name.
pub fn relationship_type_name(
    connection: &Connection,
    id: RelationshipTypeId,
) -> StorageResult<Option<String>> {
    name_for_id(connection, "_lithograph_rel_types", id)
}

/// Resolves a Property Key dictionary id to its exact stored name.
pub fn property_key_name(
    connection: &Connection,
    id: PropertyKeyId,
) -> StorageResult<Option<String>> {
    name_for_id(connection, "_lithograph_prop_keys", id)
}

pub(crate) fn allocate_layer_id(connection: &Connection) -> StorageResult<i64> {
    allocate_id(connection, 6)
}

fn allocate_id(connection: &Connection, kind: i64) -> StorageResult<i64> {
    let current: i64 = connection
        .query_row(
            "SELECT next_id FROM main._lithograph_sequences WHERE kind = ?1",
            [kind],
            |row| row.get(0),
        )
        .map_err(StorageError::from)?;
    if current <= 0 || current == i64::MAX {
        return Err(StorageError::corrupt(format!(
            "sequence {kind} cannot allocate another positive INTEGER64 identity"
        )));
    }
    let changed = connection.execute(
        "UPDATE main._lithograph_sequences SET next_id = ?2 WHERE kind = ?1 AND next_id = ?2 - 1",
        params![kind, current + 1],
    )?;
    if changed != 1 {
        return Err(StorageError::corrupt(format!(
            "sequence {kind} changed while allocating an identity"
        )));
    }
    Ok(current)
}

fn id_is_allocated(connection: &Connection, kind: i64, id: i64) -> StorageResult<bool> {
    let next_id: i64 = connection.query_row(
        "SELECT next_id FROM main._lithograph_sequences WHERE kind = ?1",
        [kind],
        |row| row.get(0),
    )?;
    Ok(id > 0 && id < next_id)
}

fn intern_name(
    connection: &Connection,
    table: &str,
    sequence: i64,
    name: &str,
) -> StorageResult<i64> {
    let select = format!("SELECT id FROM main.{table} WHERE name = ?1");
    if let Some(id) = connection
        .query_row(&select, [name], |row| row.get::<_, i64>(0))
        .optional()?
    {
        return Ok(id);
    }

    let id = allocate_id(connection, sequence)?;
    let insert = format!("INSERT INTO main.{table}(id, name) VALUES(?1, ?2)");
    connection.execute(&insert, params![id, name])?;
    Ok(id)
}

fn find_name(connection: &Connection, table: &str, name: &str) -> StorageResult<Option<i64>> {
    let select = format!("SELECT id FROM main.{table} WHERE name = ?1");
    connection
        .query_row(&select, [name], |row| row.get::<_, i64>(0))
        .optional()
        .map_err(StorageError::from)
}

fn name_for_id(connection: &Connection, table: &str, id: i64) -> StorageResult<Option<String>> {
    let select = format!("SELECT name FROM main.{table} WHERE id = ?1");
    connection
        .query_row(&select, [id], |row| row.get::<_, String>(0))
        .optional()
        .map_err(StorageError::from)
}
