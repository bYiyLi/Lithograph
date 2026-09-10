use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::encoding::{hash, i64_bytes, optional_bytes, record};
use super::identity::allocate_layer_id;
use super::layer_read::{load_property_deltas, load_relationship_deltas};
use super::property::PropertyColumns;
use super::{
    HashId, LabelId, NodeId, OwnerKind, PropertyKeyId, PropertyValue, RelationshipId,
    RelationshipTypeId, StorageError, StorageResult,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(i64)]
pub(crate) enum DeltaOp {
    Add = 1,
    Remove = 2,
}

impl DeltaOp {
    pub(super) fn from_i64(value: i64) -> StorageResult<Self> {
        match value {
            1 => Ok(Self::Add),
            2 => Ok(Self::Remove),
            _ => Err(StorageError::corrupt(format!(
                "invalid delta operation {value}"
            ))),
        }
    }
}

/// Immutable identity/endpoints/type tuple for a relationship.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub struct RelationshipRecord {
    pub id: RelationshipId,
    pub source: NodeId,
    pub type_id: RelationshipTypeId,
    pub target: NodeId,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PropertyDelta {
    pub(crate) op: DeltaOp,
    pub(crate) value: Option<PropertyValue>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct RelationshipDelta {
    pub(crate) op: DeltaOp,
    pub(crate) record: RelationshipRecord,
}

/// Canonicalized set of graph mutations relative to a Commit first parent.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct LayerBuilder {
    pub(crate) nodes: BTreeMap<NodeId, DeltaOp>,
    pub(crate) labels: BTreeMap<(NodeId, LabelId), DeltaOp>,
    pub(crate) relationships: BTreeMap<RelationshipId, RelationshipDelta>,
    pub(crate) properties: BTreeMap<(OwnerKind, i64, PropertyKeyId), PropertyDelta>,
}

impl LayerBuilder {
    /// Adds a Node logical slot to this Layer.
    pub fn add_node(&mut self, node_id: NodeId) -> StorageResult<()> {
        require_positive(node_id, "NodeId")?;
        self.nodes.insert(node_id, DeltaOp::Add);
        Ok(())
    }

    /// Removes a Node logical slot from this Layer.
    pub fn remove_node(&mut self, node_id: NodeId) -> StorageResult<()> {
        require_positive(node_id, "NodeId")?;
        self.nodes.insert(node_id, DeltaOp::Remove);
        Ok(())
    }

    /// Adds a Label membership logical slot.
    pub fn add_label(&mut self, node_id: NodeId, label_id: LabelId) -> StorageResult<()> {
        require_positive_pair(node_id, label_id, "NodeId", "LabelId")?;
        self.labels.insert((node_id, label_id), DeltaOp::Add);
        Ok(())
    }

    /// Removes a Label membership logical slot.
    pub fn remove_label(&mut self, node_id: NodeId, label_id: LabelId) -> StorageResult<()> {
        require_positive_pair(node_id, label_id, "NodeId", "LabelId")?;
        self.labels.insert((node_id, label_id), DeltaOp::Remove);
        Ok(())
    }

    /// Adds a Relationship, including self-loops and parallel relationships.
    pub fn add_relationship(&mut self, relationship: RelationshipRecord) -> StorageResult<()> {
        validate_relationship(relationship)?;
        self.relationships.insert(
            relationship.id,
            RelationshipDelta {
                op: DeltaOp::Add,
                record: relationship,
            },
        );
        Ok(())
    }

    /// Removes a Relationship while retaining its immutable adjacency tuple.
    pub fn remove_relationship(&mut self, relationship: RelationshipRecord) -> StorageResult<()> {
        validate_relationship(relationship)?;
        self.relationships.insert(
            relationship.id,
            RelationshipDelta {
                op: DeltaOp::Remove,
                record: relationship,
            },
        );
        Ok(())
    }

    /// Sets a typed property value on a Node or Relationship logical slot.
    pub fn set_property(
        &mut self,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: PropertyKeyId,
        value: PropertyValue,
    ) -> StorageResult<()> {
        require_positive_pair(owner_id, key_id, "owner id", "PropertyKeyId")?;
        self.properties.insert(
            (owner_kind, owner_id, key_id),
            PropertyDelta {
                op: DeltaOp::Add,
                value: Some(value),
            },
        );
        Ok(())
    }

    /// Removes a property logical slot; no NULL property is persisted.
    pub fn remove_property(
        &mut self,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: PropertyKeyId,
    ) -> StorageResult<()> {
        require_positive_pair(owner_id, key_id, "owner id", "PropertyKeyId")?;
        self.properties.insert(
            (owner_kind, owner_id, key_id),
            PropertyDelta {
                op: DeltaOp::Remove,
                value: None,
            },
        );
        Ok(())
    }

    /// Returns the storage-format-1 canonical LCE1 bytes for this Layer.
    pub fn canonical_lce1(&self) -> StorageResult<Vec<u8>> {
        let mut fields = Vec::with_capacity(self.slot_count());
        fields.extend(self.nodes.iter().map(|(id, op)| node_record(*id, *op)));
        fields.extend(
            self.labels
                .iter()
                .map(|((node, label), op)| label_record(*node, *label, *op)),
        );
        fields.extend(
            self.relationships
                .values()
                .map(|delta| relationship_record(*delta)),
        );
        for ((owner_kind, owner_id, key_id), delta) in &self.properties {
            fields.push(property_record(*owner_kind, *owner_id, *key_id, delta)?);
        }
        Ok(record("LAYER", &fields))
    }

    /// Returns the BLAKE3 content hash of canonical LCE1 bytes.
    pub fn content_hash(&self) -> StorageResult<HashId> {
        Ok(hash(&self.canonical_lce1()?))
    }

    fn slot_count(&self) -> usize {
        self.nodes.len() + self.labels.len() + self.relationships.len() + self.properties.len()
    }
}

pub(crate) fn persist_layer(
    connection: &Connection,
    layer: &LayerBuilder,
) -> StorageResult<(i64, HashId)> {
    let canonical = layer.canonical_lce1()?;
    let content_hash = hash(&canonical);
    if let Some(id) = connection
        .query_row(
            "SELECT id FROM main._lithograph_layers WHERE hash = ?1",
            [content_hash.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
    {
        let persisted = load_layer(connection, id)?;
        if persisted.canonical_lce1()? != canonical || persisted.content_hash()? != content_hash {
            return Err(StorageError::corrupt(format!(
                "Layer {id} does not match its persisted content hash"
            )));
        }
        return Ok((id, content_hash));
    }

    let layer_id = allocate_layer_id(connection)?;
    connection.execute(
        "INSERT INTO main._lithograph_layers(id, hash) VALUES(?1, ?2)",
        params![layer_id, content_hash.as_bytes().as_slice()],
    )?;
    persist_node_deltas(connection, layer_id, layer)?;
    persist_label_deltas(connection, layer_id, layer)?;
    persist_relationship_deltas(connection, layer_id, layer)?;
    persist_property_deltas(connection, layer_id, layer)?;
    Ok((layer_id, content_hash))
}

pub(crate) fn load_layer(connection: &Connection, layer_id: i64) -> StorageResult<LayerBuilder> {
    let mut layer = LayerBuilder::default();
    load_node_deltas(connection, layer_id, &mut layer)?;
    load_label_deltas(connection, layer_id, &mut layer)?;
    load_relationship_deltas(connection, layer_id, &mut layer)?;
    load_property_deltas(connection, layer_id, &mut layer)?;
    Ok(layer)
}

fn persist_node_deltas(
    connection: &Connection,
    layer_id: i64,
    layer: &LayerBuilder,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_node_delta(layer_id, node_id, op) VALUES(?1, ?2, ?3)",
    )?;
    for (node_id, op) in &layer.nodes {
        statement.execute(params![layer_id, node_id, *op as i64])?;
    }
    Ok(())
}

fn persist_label_deltas(
    connection: &Connection,
    layer_id: i64,
    layer: &LayerBuilder,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_label_delta(layer_id, node_id, label_id, op) VALUES(?1, ?2, ?3, ?4)",
    )?;
    for ((node_id, label_id), op) in &layer.labels {
        statement.execute(params![layer_id, node_id, label_id, *op as i64])?;
    }
    Ok(())
}

fn persist_relationship_deltas(
    connection: &Connection,
    layer_id: i64,
    layer: &LayerBuilder,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_rel_delta(layer_id, relationship_id, source_id, type_id, target_id, op) VALUES(?1, ?2, ?3, ?4, ?5, ?6)",
    )?;
    for delta in layer.relationships.values() {
        statement.execute(params![
            layer_id,
            delta.record.id,
            delta.record.source,
            delta.record.type_id,
            delta.record.target,
            delta.op as i64
        ])?;
    }
    Ok(())
}

fn persist_property_deltas(
    connection: &Connection,
    layer_id: i64,
    layer: &LayerBuilder,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "INSERT INTO main._lithograph_property_delta(layer_id, owner_kind, owner_id, key_id, op, type_tag, int_value, real_value, text_value, blob_value, aux_value) VALUES(?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)",
    )?;
    for ((owner_kind, owner_id, key_id), delta) in &layer.properties {
        let columns = delta
            .value
            .as_ref()
            .map(PropertyColumns::from_value)
            .transpose()?;
        statement.execute(params![
            layer_id,
            *owner_kind as i64,
            owner_id,
            key_id,
            delta.op as i64,
            columns.as_ref().map(|value| value.type_tag),
            columns.as_ref().and_then(|value| value.int_value),
            columns.as_ref().and_then(|value| value.real_value),
            columns
                .as_ref()
                .and_then(|value| value.text_value.as_deref()),
            columns
                .as_ref()
                .and_then(|value| value.blob_value.as_deref()),
            columns
                .as_ref()
                .and_then(|value| value.aux_value.as_deref()),
        ])?;
    }
    Ok(())
}

fn load_node_deltas(
    connection: &Connection,
    layer_id: i64,
    layer: &mut LayerBuilder,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "SELECT node_id, op FROM main._lithograph_node_delta WHERE layer_id = ?1 ORDER BY node_id",
    )?;
    let rows = statement.query_map([layer_id], |row| {
        Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?))
    })?;
    for row in rows {
        let (node_id, op) = row?;
        layer.nodes.insert(node_id, DeltaOp::from_i64(op)?);
    }
    Ok(())
}

fn load_label_deltas(
    connection: &Connection,
    layer_id: i64,
    layer: &mut LayerBuilder,
) -> StorageResult<()> {
    let mut statement = connection.prepare(
        "SELECT node_id, label_id, op FROM main._lithograph_label_delta WHERE layer_id = ?1 ORDER BY node_id, label_id",
    )?;
    let rows = statement.query_map([layer_id], |row| {
        Ok((
            row.get::<_, i64>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
        ))
    })?;
    for row in rows {
        let (node_id, label_id, op) = row?;
        layer
            .labels
            .insert((node_id, label_id), DeltaOp::from_i64(op)?);
    }
    Ok(())
}

fn node_record(node_id: NodeId, op: DeltaOp) -> Vec<u8> {
    record("NODE", &[i64_bytes(node_id), i64_bytes(op as i64)])
}

fn label_record(node_id: NodeId, label_id: LabelId, op: DeltaOp) -> Vec<u8> {
    record(
        "LABEL",
        &[
            i64_bytes(node_id),
            i64_bytes(label_id),
            i64_bytes(op as i64),
        ],
    )
}

fn relationship_record(delta: RelationshipDelta) -> Vec<u8> {
    record(
        "REL",
        &[
            i64_bytes(delta.record.id),
            i64_bytes(delta.record.source),
            i64_bytes(delta.record.type_id),
            i64_bytes(delta.record.target),
            i64_bytes(delta.op as i64),
        ],
    )
}

fn property_record(
    owner_kind: OwnerKind,
    owner_id: i64,
    key_id: PropertyKeyId,
    delta: &PropertyDelta,
) -> StorageResult<Vec<u8>> {
    let type_bytes = delta
        .value
        .as_ref()
        .map(|value| i64_bytes(value.type_tag()));
    let value_bytes = delta
        .value
        .as_ref()
        .map(PropertyValue::canonical_bytes)
        .transpose()?;
    Ok(record(
        "PROPERTY",
        &[
            i64_bytes(owner_kind as i64),
            i64_bytes(owner_id),
            i64_bytes(key_id),
            i64_bytes(delta.op as i64),
            optional_bytes(type_bytes.as_deref()),
            optional_bytes(value_bytes.as_deref()),
        ],
    ))
}

fn validate_relationship(relationship: RelationshipRecord) -> StorageResult<()> {
    require_positive(relationship.id, "RelationshipId")?;
    require_positive(relationship.source, "source NodeId")?;
    require_positive(relationship.type_id, "RelationshipTypeId")?;
    require_positive(relationship.target, "target NodeId")
}

fn require_positive(value: i64, name: &str) -> StorageResult<()> {
    if value > 0 {
        Ok(())
    } else {
        Err(StorageError::corrupt(format!(
            "{name} must be a positive INTEGER64"
        )))
    }
}

fn require_positive_pair(
    first: i64,
    second: i64,
    first_name: &str,
    second_name: &str,
) -> StorageResult<()> {
    require_positive(first, first_name)?;
    require_positive(second, second_name)
}
