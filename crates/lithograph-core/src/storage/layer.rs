use std::collections::BTreeMap;

use rusqlite::{Connection, OptionalExtension, params};

use super::encoding::{RecordHasher, i64_bytes, optional_bytes, record};
use super::identity::allocate_layer_id;
use super::layer_read::{
    load_label_deltas, load_node_deltas, load_property_deltas, load_relationship_deltas,
    visit_label_deltas, visit_node_deltas, visit_property_deltas, visit_relationship_deltas,
};
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

/// Net logical-slot counts carried by one canonical Layer.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct LayerDeltaCounts {
    pub nodes_created: u64,
    pub nodes_deleted: u64,
    pub relationships_created: u64,
    pub relationships_deleted: u64,
    pub properties_set: u64,
    pub properties_removed: u64,
    pub labels_added: u64,
    pub labels_removed: u64,
}

impl LayerBuilder {
    pub(crate) fn is_empty(&self) -> bool {
        self.nodes.is_empty()
            && self.labels.is_empty()
            && self.relationships.is_empty()
            && self.properties.is_empty()
    }

    pub(crate) fn is_property_only(&self) -> bool {
        self.nodes.is_empty() && self.labels.is_empty() && self.relationships.is_empty()
    }

    pub(crate) fn node_changes(&self) -> impl Iterator<Item = (NodeId, bool)> + '_ {
        self.nodes
            .iter()
            .map(|(node_id, op)| (*node_id, *op == DeltaOp::Add))
    }

    pub(crate) fn label_changes(&self) -> impl Iterator<Item = (NodeId, LabelId, bool)> + '_ {
        self.labels
            .iter()
            .map(|((node_id, label_id), op)| (*node_id, *label_id, *op == DeltaOp::Add))
    }

    pub(crate) fn relationship_changes(
        &self,
    ) -> impl Iterator<Item = (RelationshipRecord, bool)> + '_ {
        self.relationships
            .values()
            .map(|delta| (delta.record, delta.op == DeltaOp::Add))
    }

    pub(crate) fn property_changes(
        &self,
    ) -> impl Iterator<Item = (OwnerKind, i64, PropertyKeyId, Option<&PropertyValue>)> {
        self.properties
            .iter()
            .map(|((owner, owner_id, key_id), delta)| {
                (*owner, *owner_id, *key_id, delta.value.as_ref())
            })
    }

    /// Returns net counters for the logical slots represented by this Layer.
    pub fn delta_counts(&self) -> LayerDeltaCounts {
        let mut counts = LayerDeltaCounts::default();
        for op in self.nodes.values() {
            match op {
                DeltaOp::Add => counts.nodes_created += 1,
                DeltaOp::Remove => counts.nodes_deleted += 1,
            }
        }
        for op in self.labels.values() {
            match op {
                DeltaOp::Add => counts.labels_added += 1,
                DeltaOp::Remove => counts.labels_removed += 1,
            }
        }
        for delta in self.relationships.values() {
            match delta.op {
                DeltaOp::Add => counts.relationships_created += 1,
                DeltaOp::Remove => counts.relationships_deleted += 1,
            }
        }
        for delta in self.properties.values() {
            match delta.op {
                DeltaOp::Add => counts.properties_set += 1,
                DeltaOp::Remove => counts.properties_removed += 1,
            }
        }
        counts
    }

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
        let mut hasher = RecordHasher::new("LAYER", self.slot_count());
        for (id, op) in &self.nodes {
            hasher.field(&node_record(*id, *op));
        }
        for ((node, label), op) in &self.labels {
            hasher.field(&label_record(*node, *label, *op));
        }
        for delta in self.relationships.values() {
            hasher.field(&relationship_record(*delta));
        }
        for ((owner_kind, owner_id, key_id), delta) in &self.properties {
            hasher.field(&property_record(*owner_kind, *owner_id, *key_id, delta)?);
        }
        Ok(hasher.finish())
    }

    fn slot_count(&self) -> usize {
        self.nodes.len() + self.labels.len() + self.relationships.len() + self.properties.len()
    }
}

pub(crate) fn persist_layer(
    connection: &Connection,
    layer: &LayerBuilder,
) -> StorageResult<(i64, HashId)> {
    let content_hash = layer.content_hash()?;
    if let Some(id) = existing_layer_id(connection, content_hash)? {
        verify_existing_layer(connection, layer, id, content_hash)?;
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

fn existing_layer_id(connection: &Connection, content_hash: HashId) -> StorageResult<Option<i64>> {
    connection
        .query_row(
            "SELECT id FROM main._lithograph_layers WHERE hash = ?1",
            [content_hash.as_bytes().as_slice()],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .map_err(Into::into)
}

fn verify_existing_layer(
    connection: &Connection,
    layer: &LayerBuilder,
    id: i64,
    content_hash: HashId,
) -> StorageResult<()> {
    let canonical = layer.canonical_lce1()?;
    let persisted = load_layer(connection, id)?;
    if persisted.canonical_lce1()? != canonical || persisted.content_hash()? != content_hash {
        return Err(StorageError::corrupt(format!(
            "Layer {id} does not match its persisted content hash"
        )));
    }
    Ok(())
}

pub(crate) fn load_layer(connection: &Connection, layer_id: i64) -> StorageResult<LayerBuilder> {
    let mut layer = LayerBuilder::default();
    load_node_deltas(connection, layer_id, &mut layer)?;
    load_label_deltas(connection, layer_id, &mut layer)?;
    load_relationship_deltas(connection, layer_id, &mut layer)?;
    load_property_deltas(connection, layer_id, &mut layer)?;
    Ok(layer)
}

#[derive(Debug, Clone, Copy, Default)]
pub(super) struct StoredLayerReferenceMaxima {
    pub(super) node_id: i64,
    pub(super) relationship_id: i64,
    pub(super) label_id: i64,
    pub(super) relationship_type_id: i64,
    pub(super) property_key_id: i64,
}

pub(super) fn stored_layer_integrity(
    connection: &Connection,
    layer_id: i64,
) -> StorageResult<(HashId, StoredLayerReferenceMaxima)> {
    let slot_count: i64 = connection.query_row(
        "SELECT \
            (SELECT count(*) FROM main._lithograph_node_delta WHERE layer_id = ?1) + \
            (SELECT count(*) FROM main._lithograph_label_delta WHERE layer_id = ?1) + \
            (SELECT count(*) FROM main._lithograph_rel_delta WHERE layer_id = ?1) + \
            (SELECT count(*) FROM main._lithograph_property_delta WHERE layer_id = ?1)",
        [layer_id],
        |row| row.get(0),
    )?;
    let mut hasher = RecordHasher::new(
        "LAYER",
        usize::try_from(slot_count)
            .map_err(|_| StorageError::corrupt("Layer slot count exceeds usize"))?,
    );
    let mut maxima = StoredLayerReferenceMaxima::default();
    hash_stored_nodes(connection, layer_id, &mut hasher, &mut maxima)?;
    hash_stored_labels(connection, layer_id, &mut hasher, &mut maxima)?;
    hash_stored_relationships(connection, layer_id, &mut hasher, &mut maxima)?;
    hash_stored_properties(connection, layer_id, &mut hasher, &mut maxima)?;
    Ok((hasher.finish(), maxima))
}

fn hash_stored_nodes(
    connection: &Connection,
    layer_id: i64,
    hasher: &mut RecordHasher,
    maxima: &mut StoredLayerReferenceMaxima,
) -> StorageResult<()> {
    visit_node_deltas(connection, layer_id, |node_id, op| {
        maxima.node_id = maxima.node_id.max(node_id);
        hasher.field(&node_record(node_id, op));
        Ok(())
    })
}

fn hash_stored_labels(
    connection: &Connection,
    layer_id: i64,
    hasher: &mut RecordHasher,
    maxima: &mut StoredLayerReferenceMaxima,
) -> StorageResult<()> {
    visit_label_deltas(connection, layer_id, |node_id, label_id, op| {
        maxima.label_id = maxima.label_id.max(label_id);
        hasher.field(&label_record(node_id, label_id, op));
        Ok(())
    })
}

fn hash_stored_relationships(
    connection: &Connection,
    layer_id: i64,
    hasher: &mut RecordHasher,
    maxima: &mut StoredLayerReferenceMaxima,
) -> StorageResult<()> {
    visit_relationship_deltas(connection, layer_id, |record, op| {
        maxima.relationship_id = maxima.relationship_id.max(record.id);
        maxima.relationship_type_id = maxima.relationship_type_id.max(record.type_id);
        hasher.field(&relationship_record(RelationshipDelta { op, record }));
        Ok(())
    })
}

fn hash_stored_properties(
    connection: &Connection,
    layer_id: i64,
    hasher: &mut RecordHasher,
    maxima: &mut StoredLayerReferenceMaxima,
) -> StorageResult<()> {
    visit_property_deltas(
        connection,
        layer_id,
        |owner_kind, owner_id, key_id, delta| {
            maxima.property_key_id = maxima.property_key_id.max(key_id);
            hasher.field(&property_record(owner_kind, owner_id, key_id, &delta)?);
            Ok(())
        },
    )
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
