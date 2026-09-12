//! Canonical versioned Schema state stored inside immutable Schema objects.

use super::encoding::{parse_record, record};
use super::{HashId, PropertyValue, StorageError, StorageResult, VectorCoordinateType};
use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SchemaState {
    pub graph_nodes: BTreeMap<String, GraphNodeType>,
    pub graph_relationships: BTreeMap<String, GraphRelationshipType>,
    pub constraints: BTreeMap<String, ConstraintDefinition>,
    pub indexes: BTreeMap<String, IndexDefinition>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphNodeType {
    pub label: String,
    pub implied_labels: BTreeSet<String>,
    pub properties: BTreeMap<String, PropertyRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphRelationshipType {
    pub relationship_type: String,
    pub source_label: Option<String>,
    pub source_identifying: bool,
    pub target_label: Option<String>,
    pub target_identifying: bool,
    pub properties: BTreeMap<String, PropertyRule>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PropertyRule {
    pub property_type: PropertyType,
    pub required: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "type")]
pub enum PropertyType {
    Any,
    Boolean,
    Integer,
    Float,
    String,
    Date,
    LocalTime,
    ZonedTime,
    LocalDateTime,
    ZonedDateTime,
    Duration,
    Point,
    Uuid,
    Vector { coordinate: String, dimension: u64 },
    List { element: Box<PropertyType> },
    Union { members: Vec<PropertyType> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstraintDefinition {
    pub name: String,
    pub target: SchemaTarget,
    pub properties: Vec<String>,
    pub kind: ConstraintDefinitionKind,
    pub origin: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "kind")]
pub enum ConstraintDefinitionKind {
    Key,
    Unique,
    NotNull,
    Type { rule: PropertyRule },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "target")]
pub enum SchemaTarget {
    Node { label: String },
    Relationship { relationship_type: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StandardIndexKind {
    Lookup,
    Range,
    Text,
    Point,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexDefinition {
    pub name: String,
    pub kind: StandardIndexKind,
    pub target: IndexTarget,
    pub owning_constraint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "target")]
pub enum IndexTarget {
    NodeLookup,
    RelationshipLookup,
    NodeProperties {
        label: String,
        properties: Vec<String>,
    },
    RelationshipProperties {
        relationship_type: String,
        properties: Vec<String>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SchemaSlotChange {
    Added { slot: String },
    Removed { slot: String },
    Updated { slot: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", tag = "object")]
enum SchemaObject {
    GraphNode { definition: GraphNodeType },
    GraphRelationship { definition: GraphRelationshipType },
    Constraint { definition: ConstraintDefinition },
    Index { definition: IndexDefinition },
}

impl SchemaState {
    pub fn load(connection: &Connection, commit: HashId) -> StorageResult<Self> {
        let hash = super::schema_hash_for_commit(connection, commit)?;
        let blob = super::load_schema_blob(connection, hash)?;
        Self::from_canonical_blob(&blob)
    }

    pub fn persist(&self, connection: &Connection) -> StorageResult<HashId> {
        super::persist_schema_blob(connection, &self.canonical_blob()?)
    }

    pub fn canonical_blob(&self) -> StorageResult<Vec<u8>> {
        let objects = self.logical_objects()?;
        Ok(record("SCHEMA", &objects.into_values().collect::<Vec<_>>()))
    }

    pub fn from_canonical_blob(blob: &[u8]) -> StorageResult<Self> {
        let mut state = Self::default();
        let mut previous_slot: Option<String> = None;
        for field in parse_record(blob, "SCHEMA")? {
            let object: SchemaObject = serde_json::from_slice(field).map_err(|error| {
                StorageError::corrupt(format!("invalid Schema object JSON: {error}"))
            })?;
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
        if state.canonical_blob()? != blob {
            return Err(StorageError::corrupt(
                "Schema object is not in canonical LCE1 form",
            ));
        }
        Ok(state)
    }

    pub fn logical_slots(&self) -> StorageResult<BTreeMap<String, Vec<u8>>> {
        self.logical_objects()
    }

    pub fn diff(&self, other: &Self) -> StorageResult<Vec<SchemaSlotChange>> {
        let left = self.logical_slots()?;
        let right = other.logical_slots()?;
        let mut slots = BTreeSet::new();
        slots.extend(left.keys().cloned());
        slots.extend(right.keys().cloned());
        let mut changes = Vec::new();
        for slot in slots {
            match (left.get(&slot), right.get(&slot)) {
                (None, Some(_)) => changes.push(SchemaSlotChange::Added { slot }),
                (Some(_), None) => changes.push(SchemaSlotChange::Removed { slot }),
                (Some(before), Some(after)) if before != after => {
                    changes.push(SchemaSlotChange::Updated { slot })
                }
                _ => {}
            }
        }
        Ok(changes)
    }

    fn logical_objects(&self) -> StorageResult<BTreeMap<String, Vec<u8>>> {
        let mut objects = BTreeMap::new();
        for definition in self.graph_nodes.values() {
            insert_logical_object(
                &mut objects,
                graph_node_slot(&definition.label),
                SchemaObject::GraphNode {
                    definition: definition.clone(),
                },
            )?;
        }
        for definition in self.graph_relationships.values() {
            insert_logical_object(
                &mut objects,
                graph_relationship_slot(&definition.relationship_type),
                SchemaObject::GraphRelationship {
                    definition: definition.clone(),
                },
            )?;
        }
        for definition in self.constraints.values() {
            insert_logical_object(
                &mut objects,
                constraint_slot(&definition.name),
                SchemaObject::Constraint {
                    definition: definition.clone(),
                },
            )?;
        }
        for definition in self.indexes.values() {
            insert_logical_object(
                &mut objects,
                index_slot(&definition.name),
                SchemaObject::Index {
                    definition: definition.clone(),
                },
            )?;
        }
        Ok(objects)
    }

    fn insert_object(&mut self, object: SchemaObject) -> StorageResult<()> {
        match object {
            SchemaObject::GraphNode { definition } => {
                let key = definition.label.clone();
                if self.graph_nodes.insert(key.clone(), definition).is_some() {
                    return Err(StorageError::corrupt(format!(
                        "duplicate Graph Node Type {key:?}"
                    )));
                }
            }
            SchemaObject::GraphRelationship { definition } => {
                let key = definition.relationship_type.clone();
                if self
                    .graph_relationships
                    .insert(key.clone(), definition)
                    .is_some()
                {
                    return Err(StorageError::corrupt(format!(
                        "duplicate Graph Relationship Type {key:?}"
                    )));
                }
            }
            SchemaObject::Constraint { definition } => {
                let key = definition.name.clone();
                if self.constraints.insert(key.clone(), definition).is_some() {
                    return Err(StorageError::corrupt(format!(
                        "duplicate Constraint {key:?}"
                    )));
                }
            }
            SchemaObject::Index { definition } => {
                let key = definition.name.clone();
                if self.indexes.insert(key.clone(), definition).is_some() {
                    return Err(StorageError::corrupt(format!("duplicate Index {key:?}")));
                }
            }
        }
        Ok(())
    }
}

impl SchemaObject {
    fn slot(&self) -> String {
        match self {
            Self::GraphNode { definition } => graph_node_slot(&definition.label),
            Self::GraphRelationship { definition } => {
                graph_relationship_slot(&definition.relationship_type)
            }
            Self::Constraint { definition } => constraint_slot(&definition.name),
            Self::Index { definition } => index_slot(&definition.name),
        }
    }
}

impl PropertyRule {
    pub fn accepts(&self, value: &PropertyValue) -> bool {
        self.property_type.accepts(value)
    }
}

impl PropertyType {
    pub fn accepts(&self, value: &PropertyValue) -> bool {
        match (self, value) {
            (Self::Any, _) => true,
            (Self::Boolean, PropertyValue::Boolean(_)) => true,
            (Self::Integer, PropertyValue::Integer(_)) => true,
            (Self::Float, PropertyValue::Float(_)) => true,
            (Self::String, PropertyValue::String(_)) => true,
            (Self::Date, PropertyValue::Date(_)) => true,
            (Self::LocalTime, PropertyValue::LocalTime(_)) => true,
            (Self::ZonedTime, PropertyValue::Time { .. }) => true,
            (Self::LocalDateTime, PropertyValue::LocalDateTime { .. }) => true,
            (Self::ZonedDateTime, PropertyValue::ZonedDateTime(_)) => true,
            (Self::Duration, PropertyValue::Duration { .. }) => true,
            (Self::Point, PropertyValue::Point(_)) => true,
            (Self::Uuid, PropertyValue::Uuid(_)) => true,
            (
                Self::Vector {
                    coordinate,
                    dimension,
                },
                PropertyValue::Vector(vector),
            ) => {
                *dimension == vector.dimension
                    && vector_coordinate_name(vector.coordinate_type)
                        .eq_ignore_ascii_case(coordinate)
            }
            (Self::List { element }, PropertyValue::List(values)) => {
                values.iter().all(|value| element.accepts(value))
            }
            (Self::Union { members }, value) => members.iter().any(|member| member.accepts(value)),
            _ => false,
        }
    }
}

impl ConstraintDefinition {
    pub fn backing_index(&self) -> Option<IndexDefinition> {
        if !matches!(
            self.kind,
            ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique
        ) {
            return None;
        }
        let target = match &self.target {
            SchemaTarget::Node { label } => IndexTarget::NodeProperties {
                label: label.clone(),
                properties: self.properties.clone(),
            },
            SchemaTarget::Relationship { relationship_type } => {
                IndexTarget::RelationshipProperties {
                    relationship_type: relationship_type.clone(),
                    properties: self.properties.clone(),
                }
            }
        };
        Some(IndexDefinition {
            name: self.name.clone(),
            kind: StandardIndexKind::Range,
            target,
            owning_constraint: Some(self.name.clone()),
        })
    }
}

pub fn graph_node_slot(label: &str) -> String {
    format!("graph/node/{label}")
}
pub fn graph_relationship_slot(relationship_type: &str) -> String {
    format!("graph/relationship/{relationship_type}")
}
pub fn constraint_slot(name: &str) -> String {
    format!("constraint/{name}")
}
pub fn index_slot(name: &str) -> String {
    format!("index/{name}")
}

fn insert_logical_object(
    objects: &mut BTreeMap<String, Vec<u8>>,
    slot: String,
    object: SchemaObject,
) -> StorageResult<()> {
    let value = serde_json::to_value(object).map_err(|error| {
        StorageError::corrupt(format!("failed to encode Schema object: {error}"))
    })?;
    let bytes = serde_json::to_vec(&canonical_json(value)).map_err(|error| {
        StorageError::corrupt(format!("failed to encode Schema object: {error}"))
    })?;
    if objects.insert(slot.clone(), bytes).is_some() {
        return Err(StorageError::corrupt(format!(
            "duplicate canonical Schema slot {slot:?}"
        )));
    }
    Ok(())
}

fn canonical_json(value: JsonValue) -> JsonValue {
    match value {
        JsonValue::Array(values) => {
            JsonValue::Array(values.into_iter().map(canonical_json).collect())
        }
        JsonValue::Object(values) => {
            let mut ordered = BTreeMap::new();
            for (key, value) in values {
                ordered.insert(key, canonical_json(value));
            }
            JsonValue::Object(ordered.into_iter().collect())
        }
        value => value,
    }
}

fn vector_coordinate_name(value: VectorCoordinateType) -> &'static str {
    match value {
        VectorCoordinateType::I8 => "INTEGER8",
        VectorCoordinateType::I16 => "INTEGER16",
        VectorCoordinateType::I32 => "INTEGER32",
        VectorCoordinateType::I64 => "INTEGER64",
        VectorCoordinateType::F32 => "FLOAT32",
        VectorCoordinateType::F64 => "FLOAT64",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn empty_schema_keeps_frozen_root_encoding() {
        assert_eq!(
            SchemaState::default().canonical_blob().expect("encode"),
            record("SCHEMA", &[])
        );
    }

    #[test]
    fn canonical_schema_round_trips_and_diffs_by_slot() {
        let mut schema = SchemaState::default();
        schema.graph_nodes.insert(
            "Person".to_owned(),
            GraphNodeType {
                label: "Person".to_owned(),
                implied_labels: BTreeSet::from(["Entity".to_owned()]),
                properties: BTreeMap::from([(
                    "name".to_owned(),
                    PropertyRule {
                        property_type: PropertyType::String,
                        required: false,
                    },
                )]),
            },
        );
        let blob = schema.canonical_blob().expect("canonical blob");
        assert_eq!(
            SchemaState::from_canonical_blob(&blob).expect("decode"),
            schema
        );
        let mut next = schema.clone();
        next.indexes.insert(
            "person_name".to_owned(),
            IndexDefinition {
                name: "person_name".to_owned(),
                kind: StandardIndexKind::Range,
                target: IndexTarget::NodeProperties {
                    label: "Person".to_owned(),
                    properties: vec!["name".to_owned()],
                },
                owning_constraint: None,
            },
        );
        assert_eq!(
            schema.diff(&next).expect("diff"),
            vec![SchemaSlotChange::Added {
                slot: "index/person_name".to_owned()
            }]
        );
    }
}
