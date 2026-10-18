//! Resource observation for decoding immutable Schema objects.

use std::cell::{Cell, RefCell};
use std::fmt::Debug;
use std::io::{Read, Write};
use std::mem::size_of;
use std::rc::Rc;

use super::*;
use crate::storage::encoding::{borrowed_record_fields, equal_bytes_checked, record_checked};
use crate::storage::snapshot_read::{SnapshotReadObserver, resource_overflow};

type Check<'a> = &'a dyn Fn() -> StorageResult<()>;

#[derive(Serialize)]
#[serde(rename_all = "snake_case", tag = "object")]
enum SchemaObjectRef<'a> {
    GraphNode {
        definition: &'a GraphNodeType,
    },
    GraphRelationship {
        definition: &'a GraphRelationshipType,
    },
    Constraint {
        definition: &'a ConstraintDefinition,
    },
    Index {
        definition: &'a IndexDefinition,
    },
}

impl IndexDefinition {
    /// Compare the persisted identity while retaining bounded traversal and
    /// copy checkpoints. The caller reserves both canonical outputs first.
    pub(crate) fn canonical_eq_checked(
        &self,
        other: &Self,
        check: Check<'_>,
    ) -> StorageResult<bool> {
        let left = encode_object_checked(SchemaObjectRef::Index { definition: self }, check)?;
        let right = encode_object_checked(SchemaObjectRef::Index { definition: other }, check)?;
        equal_bytes_checked(&left, &right, check)
    }
}

impl SchemaState {
    pub(crate) fn load_observed(
        connection: &Connection,
        commit: HashId,
        observer: &dyn SnapshotReadObserver,
        check: Check<'_>,
    ) -> StorageResult<(Self, Rc<dyn Debug>)> {
        check()?;
        let hash = super::super::schema_hash_for_commit(connection, commit)?;
        let mut statement = connection.prepare(
            "SELECT canonical_blob FROM main._lithograph_schema_objects WHERE hash = ?1",
        )?;
        let mut rows = statement.query([hash.as_bytes().as_slice()])?;
        let row = rows
            .next()?
            .ok_or_else(|| StorageError::not_found(format!("Schema {}", hash.to_hex())))?;
        let blob = row.get_ref(0)?.as_blob().map_err(rusqlite::Error::from)?;
        load_blob_observed(blob, observer, check)
    }

    pub(crate) fn from_canonical_blob_checked(
        blob: &[u8],
        check: Check<'_>,
    ) -> StorageResult<Self> {
        decode_canonical_schema(blob, check)
    }

    pub(crate) fn canonical_blob_checked(&self, check: Check<'_>) -> StorageResult<Vec<u8>> {
        let objects = self.logical_objects_checked(check)?;
        let mut fields = Vec::with_capacity(objects.len());
        for bytes in objects.into_values() {
            check()?;
            fields.push(bytes);
        }
        record_checked("SCHEMA", &fields, check)
    }

    pub(super) fn logical_objects_checked(
        &self,
        check: Check<'_>,
    ) -> StorageResult<BTreeMap<String, Vec<u8>>> {
        let mut objects = BTreeMap::new();
        for definition in self.graph_nodes.values() {
            insert_checked(
                &mut objects,
                graph_node_slot(&definition.label),
                SchemaObjectRef::GraphNode { definition },
                check,
            )?;
        }
        for definition in self.graph_relationships.values() {
            insert_checked(
                &mut objects,
                graph_relationship_slot(&definition.relationship_type),
                SchemaObjectRef::GraphRelationship { definition },
                check,
            )?;
        }
        for definition in self.constraints.values() {
            insert_checked(
                &mut objects,
                constraint_slot(&definition.name),
                SchemaObjectRef::Constraint { definition },
                check,
            )?;
        }
        for definition in self.indexes.values() {
            insert_checked(
                &mut objects,
                index_slot(&definition.name),
                SchemaObjectRef::Index { definition },
                check,
            )?;
        }
        Ok(objects)
    }
}

fn load_blob_observed(
    blob: &[u8],
    observer: &dyn SnapshotReadObserver,
    check: Check<'_>,
) -> StorageResult<(SchemaState, Rc<dyn Debug>)> {
    let (retained, temporary) = schema_blob_memory_bytes(blob, check)?;
    let memory: Rc<dyn Debug> = Rc::from(observer.reserve(retained)?);
    let _temporary = observer.reserve(temporary)?;
    // Decode the borrowed SQLite value directly: no owned canonical copy is
    // necessary while this statement pins the immutable record.
    let state = SchemaState::from_canonical_blob_checked(blob, check)?;
    let actual = schema_state_memory_bytes(&state, check)?;
    if actual > retained {
        return Err(StorageError::corrupt(
            "Schema preflight underestimated decoded state",
        ));
    }
    Ok((state, memory))
}

fn decode_canonical_schema(blob: &[u8], check: Check<'_>) -> StorageResult<SchemaState> {
    check()?;
    let mut fields = borrowed_record_fields(blob, "SCHEMA")?;
    let mut state = SchemaState::default();
    let mut previous_slot: Option<String> = None;
    while let Some(field) = fields.next_field()? {
        check()?;
        let object: SchemaObject = decode_json_checked(field, check)?;
        let slot = object.slot();
        if previous_slot
            .as_ref()
            .is_some_and(|previous| previous >= &slot)
        {
            return Err(StorageError::corrupt(
                "Schema objects are not ordered by unique canonical slot",
            ));
        }
        previous_slot = Some(slot);
        state.insert_object(object)?;
    }
    fields.finish()?;
    let encoded = state.canonical_blob_checked(check)?;
    if !equal_bytes_checked(&encoded, blob, check)? {
        return Err(StorageError::corrupt(
            "Schema object is not in canonical LCE1 form",
        ));
    }
    Ok(state)
}

fn insert_checked(
    objects: &mut BTreeMap<String, Vec<u8>>,
    slot: String,
    object: SchemaObjectRef<'_>,
    check: Check<'_>,
) -> StorageResult<()> {
    let encoded = encode_object_checked(object, check)?;
    if objects.insert(slot.clone(), encoded).is_some() {
        return Err(StorageError::corrupt(format!(
            "duplicate canonical Schema slot {slot:?}"
        )));
    }
    check()
}

fn encode_object_checked(object: SchemaObjectRef<'_>, check: Check<'_>) -> StorageResult<Vec<u8>> {
    check()?;
    // serde's conversion is an indivisible third-party call; reserve all of
    // its possible owned values before entry and check on both sides.
    let value = serde_json::to_value(object).map_err(|error| {
        StorageError::corrupt(format!("failed to encode Schema object: {error}"))
    })?;
    check()?;
    let value = canonical_json_checked(value, check)?;
    let io_check = IoCheck::new(check);
    let mut writer = CheckedWriter {
        bytes: Vec::new(),
        io_check: &io_check,
    };
    let result = serde_json::to_writer(&mut writer, &value);
    io_check.finish(result)?;
    check()?;
    Ok(writer.bytes)
}

fn canonical_json_checked(value: JsonValue, check: Check<'_>) -> StorageResult<JsonValue> {
    check()?;
    match value {
        JsonValue::Array(values) => {
            let mut output = Vec::with_capacity(values.len());
            for value in values {
                output.push(canonical_json_checked(value, check)?);
            }
            Ok(JsonValue::Array(output))
        }
        JsonValue::Object(values) => {
            let mut ordered = BTreeMap::new();
            for (key, value) in values {
                ordered.insert(key, canonical_json_checked(value, check)?);
            }
            let mut output = serde_json::Map::new();
            for (key, value) in ordered {
                check()?;
                output.insert(key, value);
            }
            Ok(JsonValue::Object(output))
        }
        value => Ok(value),
    }
}

fn decode_json_checked<T: serde::de::DeserializeOwned>(
    bytes: &[u8],
    check: Check<'_>,
) -> StorageResult<T> {
    let io_check = IoCheck::new(check);
    let result = serde_json::from_reader(CheckedReader {
        bytes,
        io_check: &io_check,
    });
    io_check.finish(result)
}

struct IoCheck<'a> {
    check: Check<'a>,
    error: RefCell<Option<StorageError>>,
    processed: Cell<usize>,
}

impl<'a> IoCheck<'a> {
    fn new(check: Check<'a>) -> Self {
        Self {
            check,
            error: RefCell::new(None),
            processed: Cell::new(256),
        }
    }

    fn step(&self, amount: usize) -> std::io::Result<()> {
        if self.error.borrow().is_some() {
            return Err(std::io::Error::other("Schema processing cancelled"));
        }
        let processed = self.processed.get().saturating_add(amount);
        if processed >= 256 {
            if let Err(error) = (self.check)() {
                self.error.replace(Some(error));
                return Err(std::io::Error::other("Schema processing cancelled"));
            }
            self.processed.set(0);
        } else {
            self.processed.set(processed);
        }
        Ok(())
    }

    fn finish<T>(&self, result: Result<T, serde_json::Error>) -> StorageResult<T> {
        if let Some(error) = self.error.take() {
            return Err(error);
        }
        (self.check)()?;
        result
            .map_err(|error| StorageError::corrupt(format!("invalid Schema object JSON: {error}")))
    }
}

struct CheckedReader<'a, 'b> {
    bytes: &'a [u8],
    io_check: &'b IoCheck<'b>,
}
impl Read for CheckedReader<'_, '_> {
    fn read(&mut self, output: &mut [u8]) -> std::io::Result<usize> {
        let len = output.len().min(self.bytes.len()).min(256);
        self.io_check.step(len)?;
        output[..len].copy_from_slice(&self.bytes[..len]);
        self.bytes = &self.bytes[len..];
        Ok(len)
    }
}

struct CheckedWriter<'a> {
    bytes: Vec<u8>,
    io_check: &'a IoCheck<'a>,
}
impl Write for CheckedWriter<'_> {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        for chunk in bytes.chunks(256) {
            self.io_check.step(chunk.len())?;
            self.bytes.extend_from_slice(chunk);
        }
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn schema_blob_memory_bytes(blob: &[u8], check: Check<'_>) -> StorageResult<(u64, u64)> {
    let mut fields = borrowed_record_fields(blob, "SCHEMA")?;
    let mut structural = mul(fields.len() as u64, 256)?;
    while let Some(field) = fields.next_field()? {
        structural = add(structural, json_structure_bytes(field, check)?)?;
    }
    fields.finish()?;
    // Structural JSON tokens cover conservative B-tree/vector/enum slots.
    // Text includes serde capacity growth plus definition names and map keys.
    let retained = add(
        add(mul(blob.len() as u64, 4)?, structural)?,
        size_of::<SchemaState>() as u64,
    )?;
    // Internally-tagged serde content, typed state, canonical JSON conversion,
    // and encoding can overlap. These claims precede the first owned value.
    let temporary = add(mul(retained, 4)?, mul(blob.len() as u64, 4)?)?;
    Ok((retained, temporary))
}

fn json_structure_bytes(field: &[u8], check: Check<'_>) -> StorageResult<u64> {
    let mut state = JsonStructure {
        quoted: false,
        escaped: false,
    };
    let mut bytes = 0;
    for chunk in field.chunks(256) {
        check()?;
        for byte in chunk {
            bytes = add(bytes, state.step(*byte))?;
        }
    }
    Ok(bytes)
}

struct JsonStructure {
    quoted: bool,
    escaped: bool,
}
impl JsonStructure {
    fn step(&mut self, byte: u8) -> u64 {
        if self.escaped {
            self.escaped = false;
            return 0;
        }
        if self.quoted && byte == b'\\' {
            self.escaped = true;
        } else if byte == b'"' {
            self.quoted = !self.quoted;
        } else if !self.quoted {
            return structural_bytes(byte);
        }
        0
    }
}

fn structural_bytes(byte: u8) -> u64 {
    match byte {
        b'{' => 1_024,
        b'[' | b',' => 128,
        b':' => 256,
        _ => 0,
    }
}

fn add(left: u64, right: u64) -> StorageResult<u64> {
    left.checked_add(right).ok_or_else(resource_overflow)
}

fn mul(left: u64, right: u64) -> StorageResult<u64> {
    left.checked_mul(right).ok_or_else(resource_overflow)
}

pub(crate) fn schema_state_memory_bytes(
    schema: &SchemaState,
    check: Check<'_>,
) -> StorageResult<u64> {
    check()?;
    let mut bytes = size_of::<SchemaState>() as u64;
    add_map(&mut bytes, &schema.graph_nodes, check, |value| {
        graph_node_bytes(value, check)
    })?;
    add_map(&mut bytes, &schema.graph_relationships, check, |value| {
        graph_relationship_bytes(value, check)
    })?;
    add_map(&mut bytes, &schema.constraints, check, |value| {
        constraint_bytes(value, check)
    })?;
    add_map(&mut bytes, &schema.indexes, check, |value| {
        index_bytes(value, check)
    })?;
    Ok(bytes)
}

fn graph_node_bytes(definition: &GraphNodeType, check: Check<'_>) -> StorageResult<u64> {
    let mut heap = text_bytes(&definition.label, check)?;
    heap = add(heap, tree_bytes::<String>(definition.implied_labels.len())?)?;
    for label in &definition.implied_labels {
        heap = add(heap, text_bytes(label, check)?)?;
    }
    add(heap, rules_bytes(&definition.properties, check)?)
}

fn graph_relationship_bytes(
    definition: &GraphRelationshipType,
    check: Check<'_>,
) -> StorageResult<u64> {
    let mut heap = text_bytes(&definition.relationship_type, check)?;
    heap = add(
        heap,
        optional_text(definition.source_label.as_ref(), check)?,
    )?;
    heap = add(
        heap,
        optional_text(definition.target_label.as_ref(), check)?,
    )?;
    add(heap, rules_bytes(&definition.properties, check)?)
}

fn constraint_bytes(definition: &ConstraintDefinition, check: Check<'_>) -> StorageResult<u64> {
    let mut heap = text_bytes(&definition.name, check)?;
    heap = add(heap, target_bytes(&definition.target, check)?)?;
    heap = add(heap, texts_bytes(&definition.properties, check)?)?;
    heap = add(heap, optional_text(definition.origin.as_ref(), check)?)?;
    if let ConstraintDefinitionKind::Type { rule } = &definition.kind {
        heap = add(heap, type_bytes(&rule.property_type, check)?)?;
    }
    Ok(heap)
}

fn index_bytes(definition: &IndexDefinition, check: Check<'_>) -> StorageResult<u64> {
    let mut heap = text_bytes(&definition.name, check)?;
    heap = add(heap, index_target_bytes(&definition.target, check)?)?;
    heap = add(
        heap,
        optional_text(definition.owning_constraint.as_ref(), check)?,
    )?;
    heap = add(heap, texts_bytes(&definition.labels_or_types, check)?)?;
    heap = add(heap, texts_bytes(&definition.additional_properties, check)?)?;
    add(
        heap,
        configuration_bytes(definition.configuration.as_ref(), check)?,
    )
}

fn add_map<T>(
    bytes: &mut u64,
    values: &BTreeMap<String, T>,
    check: Check<'_>,
    heap: impl Fn(&T) -> StorageResult<u64>,
) -> StorageResult<()> {
    *bytes = add(*bytes, tree_bytes::<(String, T)>(values.len())?)?;
    for (key, value) in values {
        check()?;
        *bytes = add(add(*bytes, text_bytes(key, check)?)?, heap(value)?)?;
    }
    Ok(())
}

fn tree_bytes<T>(count: usize) -> StorageResult<u64> {
    if count == 0 {
        return Ok(0);
    }
    // BTree nodes retain up to eleven inline entries even for a small map;
    // non-root occupancy is at least five. Include child pointers and headers.
    let nodes = 1 + (count as u64 - 1) / 5;
    mul(nodes, add(mul(size_of::<T>() as u64, 11)?, 128)?)
}

fn rules_bytes(values: &BTreeMap<String, PropertyRule>, check: Check<'_>) -> StorageResult<u64> {
    let mut bytes = 0;
    add_map(&mut bytes, values, check, |rule| {
        type_bytes(&rule.property_type, check)
    })?;
    Ok(bytes)
}

fn type_bytes(value: &PropertyType, check: Check<'_>) -> StorageResult<u64> {
    check()?;
    match value {
        PropertyType::Vector { coordinate, .. } => text_bytes(coordinate, check),
        PropertyType::List { element } => add(
            size_of::<PropertyType>() as u64,
            type_bytes(element, check)?,
        ),
        PropertyType::Union { members } => {
            let mut bytes = mul(members.capacity() as u64, size_of::<PropertyType>() as u64)?;
            for member in members {
                bytes = add(bytes, type_bytes(member, check)?)?;
            }
            Ok(bytes)
        }
        _ => Ok(0),
    }
}

fn target_bytes(target: &SchemaTarget, check: Check<'_>) -> StorageResult<u64> {
    match target {
        SchemaTarget::Node { label } => text_bytes(label, check),
        SchemaTarget::Relationship { relationship_type } => text_bytes(relationship_type, check),
    }
}

fn index_target_bytes(target: &IndexTarget, check: Check<'_>) -> StorageResult<u64> {
    match target {
        IndexTarget::NodeProperties { label, properties } => {
            add(text_bytes(label, check)?, texts_bytes(properties, check)?)
        }
        IndexTarget::RelationshipProperties {
            relationship_type,
            properties,
        } => add(
            text_bytes(relationship_type, check)?,
            texts_bytes(properties, check)?,
        ),
        _ => Ok(0),
    }
}

fn configuration_bytes(
    configuration: Option<&IndexConfiguration>,
    check: Check<'_>,
) -> StorageResult<u64> {
    let Some(configuration) = configuration else {
        return Ok(0);
    };
    match configuration {
        IndexConfiguration::FullText { analyzer, .. } => text_bytes(analyzer, check),
        IndexConfiguration::Vector {
            similarity_function,
            quantization_type,
            default_search_expansion_factor,
            ..
        } => add(
            add(
                text_bytes(similarity_function, check)?,
                text_bytes(quantization_type, check)?,
            )?,
            text_bytes(default_search_expansion_factor, check)?,
        ),
        IndexConfiguration::Semantic {
            provider,
            provider_config,
            similarity_function,
            ..
        } => add(
            add(
                text_bytes(provider, check)?,
                json_heap_bytes(provider_config, check)?,
            )?,
            text_bytes(similarity_function, check)?,
        ),
    }
}

fn json_heap_bytes(value: &JsonValue, check: Check<'_>) -> StorageResult<u64> {
    check()?;
    match value {
        JsonValue::String(value) => text_bytes(value, check),
        JsonValue::Array(values) => {
            let mut bytes = mul(values.capacity() as u64, size_of::<JsonValue>() as u64)?;
            for value in values {
                bytes = add(bytes, json_heap_bytes(value, check)?)?;
            }
            Ok(bytes)
        }
        JsonValue::Object(values) => {
            let mut bytes = tree_bytes::<(String, JsonValue)>(values.len())?;
            for (key, value) in values {
                bytes = add(
                    add(bytes, text_bytes(key, check)?)?,
                    json_heap_bytes(value, check)?,
                )?;
            }
            Ok(bytes)
        }
        _ => Ok(0),
    }
}

fn text_bytes(value: &String, check: Check<'_>) -> StorageResult<u64> {
    for _ in (0..value.len()).step_by(64 * 1024) {
        check()?;
    }
    Ok(value.capacity() as u64)
}

fn optional_text(value: Option<&String>, check: Check<'_>) -> StorageResult<u64> {
    value.map_or(Ok(0), |value| text_bytes(value, check))
}

fn texts_bytes(values: &Vec<String>, check: Check<'_>) -> StorageResult<u64> {
    let mut bytes = mul(values.capacity() as u64, size_of::<String>() as u64)?;
    for value in values {
        bytes = add(bytes, text_bytes(value, check)?)?;
    }
    Ok(bytes)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::encoding::record;

    fn schema_fixture() -> SchemaState {
        let mut schema = SchemaState::default();
        schema.indexes.insert("semantic".to_owned(), IndexDefinition {
            name: "semantic".to_owned(), kind: StandardIndexKind::Semantic,
            target: IndexTarget::NodeProperties { label: "人物".to_owned(), properties: vec!["name".to_owned()] },
            owning_constraint: None, labels_or_types: vec![], additional_properties: vec![],
            configuration: Some(IndexConfiguration::Semantic {
                provider: "test".to_owned(), dimensions: 2, similarity_function: "cosine".to_owned(),
                provider_config: serde_json::json!({ "long": "人物🦀".repeat(25_000), "items": vec![0; 1_024] }),
            }),
        });
        schema
    }

    #[derive(Debug)]
    struct Budget {
        limit: u64,
        used: Rc<Cell<u64>>,
        attempts: Cell<usize>,
    }
    #[derive(Debug)]
    struct Lease {
        bytes: u64,
        used: Rc<Cell<u64>>,
    }
    impl Drop for Lease {
        fn drop(&mut self) {
            self.used.set(self.used.get() - self.bytes);
        }
    }
    impl SnapshotReadObserver for Budget {
        fn reserve(&self, bytes: u64) -> StorageResult<Box<dyn Debug>> {
            self.attempts.set(self.attempts.get() + 1);
            let used = add(self.used.get(), bytes)?;
            if used > self.limit {
                return Err(resource_overflow());
            }
            self.used.set(used);
            Ok(Box::new(Lease {
                bytes,
                used: self.used.clone(),
            }))
        }
    }

    fn schema_database(schema: &SchemaState) -> (Connection, HashId) {
        let connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch(
                "CREATE TABLE _lithograph_commits(id BLOB PRIMARY KEY, schema_hash BLOB); \
             CREATE TABLE _lithograph_schema_objects(hash BLOB PRIMARY KEY, canonical_blob BLOB);",
            )
            .unwrap();
        let commit = HashId::from_bytes([1; 32]);
        let hash = HashId::from_bytes([2; 32]);
        connection
            .execute(
                "INSERT INTO _lithograph_commits VALUES(?1,?2)",
                rusqlite::params![commit.as_bytes().as_slice(), hash.as_bytes().as_slice()],
            )
            .unwrap();
        connection
            .execute(
                "INSERT INTO _lithograph_schema_objects VALUES(?1,?2)",
                rusqlite::params![hash.as_bytes().as_slice(), schema.canonical_blob().unwrap()],
            )
            .unwrap();
        (connection, commit)
    }

    #[test]
    fn schema_preflight_covers_owned_state_and_preserves_canonical_bytes() {
        let schema = schema_fixture();
        let bytes = schema.canonical_blob().unwrap();
        let (retained, _) = schema_blob_memory_bytes(&bytes, &|| Ok(())).unwrap();
        let decoded = SchemaState::from_canonical_blob_checked(&bytes, &|| Ok(())).unwrap();
        assert!(retained >= schema_state_memory_bytes(&decoded, &|| Ok(())).unwrap());
        assert_eq!(decoded, schema);
        assert_eq!(decoded.canonical_blob_checked(&|| Ok(())).unwrap(), bytes);
        assert_eq!(decoded.clone_checked(&|| Ok(())).unwrap(), schema);
        for (slot, bytes) in decoded.logical_slots().unwrap() {
            let definition = &decoded.indexes[slot.strip_prefix("index/").unwrap()];
            assert_eq!(
                bytes,
                encode_schema_object(SchemaObject::Index {
                    definition: definition.clone()
                })
                .unwrap()
            );
        }
    }

    #[test]
    fn observed_schema_rejects_before_json_decode_and_releases_claims() {
        let schema = schema_fixture();
        let (connection, commit) = schema_database(&schema);
        let budget = Budget {
            limit: 1_024,
            used: Rc::new(Cell::new(0)),
            attempts: Cell::new(0),
        };
        let error =
            SchemaState::load_observed(&connection, commit, &budget, &|| Ok(())).unwrap_err();
        assert!(matches!(error, StorageError::Sqlite(_)));
        assert_eq!(budget.attempts.get(), 1);
        assert_eq!(budget.used.get(), 0);
        let budget = Budget {
            limit: 256 * 1024 * 1024,
            ..budget
        };
        let (decoded, lease) =
            SchemaState::load_observed(&connection, commit, &budget, &|| Ok(())).unwrap();
        assert_eq!(decoded, schema);
        assert!(budget.used.get() >= schema_state_memory_bytes(&decoded, &|| Ok(())).unwrap());
        drop(lease);
        assert_eq!(budget.used.get(), 0);
    }

    #[test]
    fn schema_encode_decode_and_clone_cancel_during_large_owned_values() {
        let schema = schema_fixture();
        let bytes = schema.canonical_blob().unwrap();
        let calls = Cell::new(0);
        let check = || {
            calls.set(calls.get() + 1);
            if calls.get() == 257 {
                Err(StorageError::not_found("cancel Schema work"))
            } else {
                Ok(())
            }
        };
        assert!(schema.canonical_blob_checked(&check).is_err());
        assert_eq!(calls.get(), 257);
        calls.set(0);
        assert!(SchemaState::from_canonical_blob_checked(&bytes, &check).is_err());
        assert_eq!(calls.get(), 257);
        calls.set(0);
        assert!(schema.clone_checked(&check).is_err());
        assert_eq!(calls.get(), 257);
        let malformed = record("SCHEMA", &[b"{bad JSON}".to_vec()]);
        assert!(SchemaState::from_canonical_blob_checked(&malformed, &|| Ok(())).is_err());
    }

    #[test]
    fn long_schema_name_capacity_is_included_before_decode() {
        let mut schema = SchemaState::default();
        let label = "long🦀".repeat(32_000);
        schema.graph_nodes.insert(
            label.clone(),
            GraphNodeType {
                label,
                implied_labels: BTreeSet::new(),
                properties: BTreeMap::new(),
            },
        );
        let blob = schema.canonical_blob().unwrap();
        let decoded = SchemaState::from_canonical_blob_checked(&blob, &|| Ok(())).unwrap();
        assert!(
            schema_blob_memory_bytes(&blob, &|| Ok(())).unwrap().0
                >= schema_state_memory_bytes(&decoded, &|| Ok(())).unwrap()
        );
    }

    #[test]
    fn checked_index_identity_preserves_frozen_json_identity_and_cancels_large_values() {
        let mut left = schema_fixture().indexes.remove("semantic").unwrap();
        let right = left.clone();
        assert!(left.canonical_eq_checked(&right, &|| Ok(())).unwrap());
        let calls = Cell::new(0);
        let check = || {
            calls.set(calls.get() + 1);
            if calls.get() == 257 {
                Err(StorageError::not_found("cancel Index identity"))
            } else {
                Ok(())
            }
        };
        assert!(left.canonical_eq_checked(&right, &check).is_err());
        assert_eq!(calls.get(), 257);
        let Some(IndexConfiguration::Semantic {
            provider_config, ..
        }) = &mut left.configuration
        else {
            panic!("semantic fixture");
        };
        *provider_config = serde_json::json!({"zero":0.0});
        let mut right = left.clone();
        let Some(IndexConfiguration::Semantic {
            provider_config, ..
        }) = &mut right.configuration
        else {
            panic!("semantic fixture");
        };
        *provider_config = serde_json::json!({"zero":-0.0});
        assert!(!left.canonical_eq(&right).unwrap());
        assert!(!left.canonical_eq_checked(&right, &|| Ok(())).unwrap());
    }
}
