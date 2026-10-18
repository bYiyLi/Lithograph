use super::*;

pub(super) struct PatchOperations {
    pub(super) values: Vec<Value>,
    pub(super) memory: MemoryReservation,
}

impl std::ops::Deref for PatchOperations {
    type Target = Vec<Value>;
    fn deref(&self) -> &Self::Target {
        &self.values
    }
}

impl std::ops::DerefMut for PatchOperations {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.values
    }
}

impl PatchOperations {
    pub(super) fn new(connection: &Connection) -> QueryResult<Self> {
        Ok(Self {
            values: Vec::new(),
            memory: connection.reserve(0)?,
        })
    }

    pub(super) fn reserve_operation(&mut self, fields: u64, text: u64) -> QueryResult<()> {
        self.memory.grow(checked_add(
            checked_mul(map_entry_bytes::<Value>()?, fields)?,
            checked_add(text, checked_mul(fields, 32)?)?,
        )?)?;
        if self.values.len() == self.values.capacity() {
            let capacity = self
                .values
                .capacity()
                .max(4)
                .checked_mul(2)
                .ok_or_else(|| {
                    crate::query::resources::memory_error("version patch size overflow")
                })?;
            let additional = capacity - self.values.capacity();
            self.memory.grow(allocation_bytes::<Value>(additional)?)?;
            self.values
                .try_reserve_exact(capacity - self.values.len())
                .map_err(|_| {
                    crate::query::resources::memory_error(
                        "cannot allocate version patch operations",
                    )
                })?;
        }
        Ok(())
    }
}

pub(super) fn row_allocation_bytes(fields: u64) -> QueryResult<u64> {
    checked_add(
        allocation_bytes::<ProcedureRow>(1)?,
        checked_add(
            checked_mul(map_entry_bytes::<Value>()?, fields)?,
            checked_mul(fields, 32)?,
        )?,
    )
}

pub(super) fn schema_core_value(
    connection: &Connection,
    schema: &SchemaState,
) -> QueryResult<(Value, MemoryReservation)> {
    #[derive(serde::Serialize)]
    struct Core<'a> {
        graph_nodes: &'a BTreeMap<String, storage::GraphNodeType>,
        graph_relationships: &'a BTreeMap<String, storage::GraphRelationshipType>,
        constraints: &'a BTreeMap<String, storage::ConstraintDefinition>,
    }
    super::super::merge::serde_value(
        connection,
        &Core {
            graph_nodes: &schema.graph_nodes,
            graph_relationships: &schema.graph_relationships,
            constraints: &schema.constraints,
        },
    )
}

pub(super) fn json_temporary(
    connection: &Connection,
    value: &Value,
) -> QueryResult<MemoryReservation> {
    let owned = value_bytes_controlled(value, connection.control())?;
    let encoded = encoded_json_bytes_controlled(value, connection.control())?;
    connection.reserve(checked_add(
        checked_mul(owned, 4)?,
        checked_mul(encoded, 2)?,
    )?)
}

pub(super) fn apply_schema_core(
    connection: &Connection,
    schema: &mut SchemaState,
    value: &Value,
) -> QueryResult<()> {
    let _temporary = json_temporary(connection, value)?;
    let json = super::super::value_to_json_controlled(value, Some(connection.control()))?;
    #[derive(serde::Deserialize)]
    struct Core {
        graph_nodes: BTreeMap<String, storage::GraphNodeType>,
        graph_relationships: BTreeMap<String, storage::GraphRelationshipType>,
        constraints: BTreeMap<String, storage::ConstraintDefinition>,
    }
    connection.control().check()?;
    let core: Core = serde_json::from_value(json).map_err(|error| {
        QueryError::invalid_argument(format!("invalid SetSchema payload: {error}"))
    })?;
    connection.control().check()?;
    schema.graph_nodes = core.graph_nodes;
    schema.graph_relationships = core.graph_relationships;
    schema.constraints = core.constraints;
    Ok(())
}

pub(super) fn index_value(
    connection: &Connection,
    index: &IndexDefinition,
) -> QueryResult<(Value, MemoryReservation)> {
    super::super::merge::serde_value(connection, index)
}

pub(super) fn same_index_controlled(
    connection: &Connection,
    left: Option<&IndexDefinition>,
    right: Option<&IndexDefinition>,
) -> QueryResult<bool> {
    match (left, right) {
        (Some(left), Some(right)) => {
            let (left, _left_memory) = index_value(connection, left)?;
            let (right, _right_memory) = index_value(connection, right)?;
            connection.control().check()?;
            let equal = super::super::canonical::same_value_controlled(connection, &left, &right)?;
            connection.control().check()?;
            Ok(equal)
        }
        (None, None) => Ok(true),
        _ => Ok(false),
    }
}

pub(super) fn index_definition(
    connection: &Connection,
    value: Option<&Value>,
) -> QueryResult<IndexDefinition> {
    let value = value
        .ok_or_else(|| QueryError::invalid_argument("Index operation is missing definition"))?;
    let _temporary = json_temporary(connection, value)?;
    let json = super::super::value_to_json_controlled(value, Some(connection.control()))?;
    connection.control().check()?;
    let result = serde_json::from_value(json).map_err(|error| {
        QueryError::invalid_argument(format!("invalid Index definition: {error}"))
    });
    connection.control().check()?;
    result
}

pub(super) fn property_state(
    connection: &Connection,
    value: Option<&PropertyValue>,
) -> QueryResult<(Value, MemoryReservation)> {
    let mut memory = connection.reserve(checked_add(
        checked_mul(map_entry_bytes::<Value>()?, 2)?,
        32,
    )?)?;
    let value = match value {
        None => Value::Map(BTreeMap::from([(
            "present".to_owned(),
            Value::Boolean(false),
        )])),
        Some(value) => {
            let (value, value_memory) = connection.property_value(value)?;
            memory.merge(value_memory)?;
            Value::Map(BTreeMap::from([
                ("present".to_owned(), Value::Boolean(true)),
                ("value".to_owned(), value),
            ]))
        }
    };
    Ok((value, memory))
}

pub(in crate::query::version) fn same_property_value_controlled(
    connection: &Connection,
    left: Option<&PropertyValue>,
    right: Option<&PropertyValue>,
) -> QueryResult<bool> {
    let (Some(left), Some(right)) = (left, right) else {
        return Ok(left.is_none() && right.is_none());
    };
    let check = || {
        connection
            .control()
            .check()
            .map_err(super::super::execution::storage_error)
    };
    let bytes = checked_add(
        storage::property_value_memory_bytes(left, &check)?,
        storage::property_value_memory_bytes(right, &check)?,
    )?;
    let _memory = connection.reserve(checked_mul(bytes, 8)?)?;
    let left = left.canonical_bytes_checked(&check)?;
    let right = right.canonical_bytes_checked(&check)?;
    super::super::canonical::same_bytes(connection, &left, &right)
}

pub(super) fn relationship_value(
    connection: &Connection,
    record: RelationshipRecord,
) -> QueryResult<(Value, MemoryReservation)> {
    storage::with_relationship_type_name(connection, record.type_id, |relationship_type| {
        let relationship_type = relationship_type.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                "Relationship type dictionary entry is missing",
            )
        })?;
        let memory = connection.reserve(checked_add(
            checked_mul(map_entry_bytes::<Value>()?, 3)?,
            checked_add(relationship_type.len() as u64, 128)?,
        )?)?;
        Ok((
            Value::Map(BTreeMap::from([
                (
                    "type".to_owned(),
                    Value::String(copy_text(connection, relationship_type)?),
                ),
                (
                    "source".to_owned(),
                    Value::String(format!("n:{}", record.source)),
                ),
                (
                    "target".to_owned(),
                    Value::String(format!("n:{}", record.target)),
                ),
            ])),
            memory,
        ))
    })
}

pub(super) fn label_name(
    connection: &Connection,
    label: i64,
) -> QueryResult<(String, MemoryReservation)> {
    storage::with_label_name(connection, label, |name| {
        let name = name.ok_or_else(|| {
            QueryError::new(
                QueryErrorKind::Storage,
                format!("LabelId {label} is missing"),
            )
        })?;
        let memory = connection.reserve(name.len() as u64)?;
        Ok((copy_text(connection, name)?, memory))
    })
}

pub(super) fn patch_envelope(
    connection: &Connection,
    before_commit: HashId,
    after_commit: HashId,
    mut operations: PatchOperations,
) -> QueryResult<VersionValue> {
    let mut statement =
        connection.prepare("SELECT database_id FROM main._lithograph_meta WHERE id = 1")?;
    let mut rows = statement.query([])?;
    let record = rows
        .next()?
        .ok_or_else(|| QueryError::internal("database metadata is missing"))?;
    let raw = record.get_ref(0)?;
    let rusqlite::types::ValueRef::Text(bytes) = raw else {
        return Err(QueryError::internal("database identity is not text"));
    };
    let database_id = std::str::from_utf8(bytes)
        .map_err(|_| QueryError::internal("database identity is not UTF-8"))?;
    operations.memory.grow(checked_add(
        checked_mul(map_entry_bytes::<Value>()?, 5)?,
        checked_add(database_id.len() as u64, 256)?,
    )?)?;
    let database_id = copy_text(connection, database_id)?;
    Ok(VersionValue {
        value: Value::Map(BTreeMap::from([
            ("format".to_owned(), Value::Integer(1)),
            ("databaseId".to_owned(), Value::String(database_id)),
            ("from".to_owned(), commit_value(before_commit)),
            ("to".to_owned(), commit_value(after_commit)),
            ("operations".to_owned(), Value::List(operations.values)),
        ])),
        memory: operations.memory,
    })
}

pub(super) fn reserve_patch_growth(
    connection: &Connection,
    state: &mut VersionState,
    value: &Value,
) -> QueryResult<()> {
    let growth = checked_add(
        checked_mul(value_bytes_controlled(value, connection.control())?, 4)?,
        encoded_json_bytes_controlled(value, connection.control())?,
    )?;
    state
        .memory
        .grow(checked_add(growth, map_entry_bytes::<Value>()?)?)?;
    Ok(())
}

/// The caller has reserved capacity; copying proceeds in bounded UTF-8 chunks.
pub(super) fn copy_text(connection: &Connection, text: &str) -> QueryResult<String> {
    let mut output = String::with_capacity(text.len());
    append_text(connection, &mut output, text)?;
    Ok(output)
}

pub(super) fn slot_text(connection: &Connection, prefix: &str, name: &str) -> QueryResult<String> {
    let length = prefix
        .len()
        .checked_add(name.len())
        .ok_or_else(|| crate::query::resources::memory_error("version slot length overflow"))?;
    let mut output = String::with_capacity(length);
    append_text(connection, &mut output, prefix)?;
    append_text(connection, &mut output, name)?;
    Ok(output)
}

fn append_text(connection: &Connection, output: &mut String, text: &str) -> QueryResult<()> {
    let mut start = 0;
    while start < text.len() {
        let mut end = start.saturating_add(64 * 1024).min(text.len());
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        connection.control().bytes((end - start) as u64)?;
        output.push_str(&text[start..end]);
        start = end;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::resources::{ExecutionControl, QueryResources};
    use std::cell::Cell;

    #[test]
    fn rejected_patch_output_does_not_grow_its_operation_vector() {
        let database = rusqlite::Connection::open_in_memory().unwrap();
        let resources = QueryResources::new(Some(128));
        let control = ExecutionControl::new(&resources, &|| false);
        let connection = Connection::new(&database, &control);
        let mut operations = PatchOperations::new(&connection).unwrap();
        let error = operations.reserve_operation(5, 128).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(operations.values.capacity(), 0);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn borrowed_patch_property_copy_keeps_and_releases_its_lease() {
        let database = rusqlite::Connection::open_in_memory().unwrap();
        let property =
            PropertyValue::List(vec![PropertyValue::String("人物".repeat(4_096)); 128]);
        let resources = QueryResources::new(Some(64 * 1024 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let connection = Connection::new(&database, &control);
        let (value, memory) = property_state(&connection, Some(&property)).unwrap();
        assert!(resources.used() > 1024 * 1024);
        assert!(matches!(value, Value::Map(_)));
        drop((value, memory));
        assert_eq!(resources.used(), 0);

        let resources = QueryResources::new(Some(64 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let connection = Connection::new(&database, &control);
        let error = property_state(&connection, Some(&property)).unwrap_err();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.used(), 0);
        assert!(resources.peak() <= 64 * 1024);
    }

    #[test]
    fn canonical_patch_property_comparison_observes_interrupt_and_releases_memory() {
        let database = rusqlite::Connection::open_in_memory().unwrap();
        let resources = QueryResources::new(Some(64 * 1024 * 1024));
        let calls = Cell::new(0);
        let interrupted = || {
            if resources.used() == 0 {
                return false;
            }
            calls.set(calls.get() + 1);
            calls.get() >= 2
        };
        let control = ExecutionControl::new(&resources, &interrupted);
        let connection = Connection::new(&database, &control);
        let property = PropertyValue::String("人物".repeat(64 * 1024));
        let error = same_property_value_controlled(&connection, Some(&property), Some(&property))
            .unwrap_err();
        assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
        assert!(resources.peak() > 0);
        assert_eq!(resources.used(), 0);
    }
}
