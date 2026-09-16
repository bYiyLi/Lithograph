//! Snapshot resolution over a checkpoint plus first-parent Layer overlay.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, Statement, params};

use super::layer::{DeltaOp, LayerBuilder, PropertyDelta, RelationshipDelta, RelationshipRecord};
use super::layer_read::{
    visit_label_deltas, visit_node_deltas, visit_property_deltas, visit_relationship_deltas,
};
use super::property::PropertyColumns;
use super::snapshot_lineage::resolve_lineage;
use super::{
    HashId, LabelId, NodeId, OwnerKind, PropertyKeyId, PropertyValue, RelationshipId,
    RelationshipTypeId, ScanPage, SchemaState, StorageError, StorageResult,
};

#[derive(Debug, Clone, Default)]
pub(super) struct Overlay {
    pub(super) nodes: BTreeMap<NodeId, DeltaOp>,
    pub(super) labels: BTreeMap<(NodeId, LabelId), DeltaOp>,
    pub(super) labels_by_label: BTreeMap<(LabelId, NodeId), DeltaOp>,
    pub(super) relationships: BTreeMap<RelationshipId, RelationshipDelta>,
    pub(super) outgoing:
        BTreeMap<(NodeId, RelationshipTypeId, NodeId, RelationshipId), RelationshipDelta>,
    pub(super) incoming:
        BTreeMap<(NodeId, RelationshipTypeId, NodeId, RelationshipId), RelationshipDelta>,
    pub(super) outgoing_by_id: BTreeMap<(NodeId, RelationshipId), RelationshipDelta>,
    pub(super) incoming_by_id: BTreeMap<(NodeId, RelationshipId), RelationshipDelta>,
    pub(super) incident_by_id: BTreeMap<(NodeId, RelationshipId), RelationshipDelta>,
    pub(super) properties: BTreeMap<(OwnerKind, i64, PropertyKeyId), PropertyDelta>,
}

/// Connection-independent graph state resolved once for one query execution.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSnapshotState {
    commit: HashId,
    cache_identity: HashId,
    checkpoint: Option<HashId>,
    overlay: Arc<Overlay>,
    pub(super) property_cache: Rc<RefCell<PropertyCache>>,
    query_local_layer: Option<Arc<LayerBuilder>>,
    schema_override: Option<Arc<SchemaState>>,
}

impl ResolvedSnapshotState {
    pub(crate) fn query_schema_state(&self) -> Option<&SchemaState> {
        self.schema_override.as_deref()
    }
}

/// Commit-pinned graph snapshot.
#[derive(Clone)]
pub struct Snapshot<'connection> {
    pub(super) connection: &'connection Connection,
    pub(super) commit: HashId,
    pub(super) cache_identity: HashId,
    pub(super) checkpoint: Option<HashId>,
    pub(super) overlay: Arc<Overlay>,
    pub(super) property_cache: Rc<RefCell<PropertyCache>>,
    pub(super) query_local_layer: Option<Arc<LayerBuilder>>,
    pub(super) schema_override: Option<Arc<SchemaState>>,
}

pub(super) type PropertyCache = BTreeMap<(OwnerKind, i64, PropertyKeyId), Option<PropertyValue>>;

impl<'connection> Snapshot<'connection> {
    /// Resolves a Commit-pinned snapshot.
    pub fn resolve(connection: &'connection Connection, commit: HashId) -> StorageResult<Self> {
        let state = Self::resolve_state_with_checkpoint_skip(connection, commit, None)?;
        Ok(Self::from_resolved_state(connection, &state))
    }

    pub(crate) fn resolve_query_state(
        connection: &Connection,
        commit: HashId,
    ) -> StorageResult<ResolvedSnapshotState> {
        let mut state = Self::resolve_state_with_checkpoint_skip(connection, commit, None)?;
        state.schema_override = Some(Arc::new(SchemaState::load(connection, commit)?));
        Ok(state)
    }

    pub(crate) fn resolve_query_state_with_layer_and_schema(
        connection: &Connection,
        commit: HashId,
        layer: &LayerBuilder,
        schema: SchemaState,
    ) -> StorageResult<ResolvedSnapshotState> {
        let mut state = Self::resolve_state_with_checkpoint_skip(connection, commit, None)?;
        Arc::make_mut(&mut state.overlay).apply(layer.clone());
        state.query_local_layer = Some(Arc::new(layer.clone()));
        state.cache_identity = query_local_cache_identity(commit, layer, Some(&schema))?;
        state.schema_override = Some(Arc::new(schema));
        Ok(state)
    }

    pub(crate) fn from_resolved_state(
        connection: &'connection Connection,
        state: &ResolvedSnapshotState,
    ) -> Self {
        Self {
            connection,
            commit: state.commit,
            cache_identity: state.cache_identity,
            checkpoint: state.checkpoint,
            overlay: state.overlay.clone(),
            property_cache: state.property_cache.clone(),
            query_local_layer: state.query_local_layer.clone(),
            schema_override: state.schema_override.clone(),
        }
    }

    /// Resolves a Commit-pinned snapshot and overlays one query-local staged Layer.
    ///
    /// The staged Layer is never persisted by this helper. Mutation execution uses
    /// it to apply Cypher clause-state semantics before the final Commit exists.
    pub(crate) fn resolve_with_layer(
        connection: &'connection Connection,
        commit: HashId,
        layer: &super::layer::LayerBuilder,
    ) -> StorageResult<Self> {
        let mut snapshot = Self::resolve(connection, commit)?;
        Arc::make_mut(&mut snapshot.overlay).apply(layer.clone());
        snapshot.query_local_layer = Some(Arc::new(layer.clone()));
        snapshot.cache_identity = query_local_cache_identity(commit, layer, None)?;
        Ok(snapshot)
    }

    pub(crate) fn apply_layer(&mut self, layer: &LayerBuilder) -> StorageResult<()> {
        let layer_hash = layer.content_hash()?;
        let previous_identity = self.cache_identity;
        Arc::make_mut(&mut self.overlay).apply(layer.clone());
        let local = self
            .query_local_layer
            .get_or_insert_with(|| Arc::new(LayerBuilder::default()));
        let local = Arc::make_mut(local);
        merge_query_local_layer(local, layer);
        self.cache_identity = advance_query_local_cache_identity(previous_identity, layer_hash);
        Ok(())
    }

    pub(crate) fn resolve_with_layer_and_schema(
        connection: &'connection Connection,
        commit: HashId,
        layer: &super::layer::LayerBuilder,
        schema: SchemaState,
    ) -> StorageResult<Self> {
        let mut snapshot = Self::resolve(connection, commit)?;
        Arc::make_mut(&mut snapshot.overlay).apply(layer.clone());
        snapshot.query_local_layer = Some(Arc::new(layer.clone()));
        snapshot.cache_identity = query_local_cache_identity(commit, layer, Some(&schema))?;
        snapshot.schema_override = Some(Arc::new(schema));
        Ok(snapshot)
    }

    fn resolve_state_with_checkpoint_skip(
        connection: &Connection,
        commit: HashId,
        skip_checkpoint: Option<HashId>,
    ) -> StorageResult<ResolvedSnapshotState> {
        let (checkpoint, layer_ids) = resolve_lineage(connection, commit, skip_checkpoint)?;
        #[cfg(feature = "test-support")]
        crate::performance::record_resolved_state(layer_ids.len());
        let mut overlay = Overlay::default();
        for layer_id in layer_ids.into_iter().rev() {
            overlay.apply_persisted_layer(connection, layer_id)?;
        }
        Ok(ResolvedSnapshotState {
            commit,
            cache_identity: commit,
            checkpoint,
            overlay: Arc::new(overlay),
            property_cache: Rc::new(RefCell::new(BTreeMap::new())),
            query_local_layer: None,
            schema_override: None,
        })
    }

    /// Commit pinned by this snapshot.
    pub fn commit(&self) -> HashId {
        self.commit
    }

    pub(crate) fn cache_identity(&self) -> HashId {
        self.cache_identity
    }

    pub(crate) fn query_local_layer(&self) -> Option<&LayerBuilder> {
        self.query_local_layer.as_deref()
    }

    pub(crate) fn schema_state(&self) -> StorageResult<SchemaState> {
        match &self.schema_override {
            Some(schema) => Ok((**schema).clone()),
            None => SchemaState::load(self.connection, self.commit),
        }
    }

    pub(crate) fn connection_for_query(&self) -> &'connection Connection {
        self.connection
    }

    /// Returns whether a Node is visible in this snapshot.
    pub fn node_exists(&self, node_id: NodeId) -> StorageResult<bool> {
        if let Some(op) = self.overlay.nodes.get(&node_id) {
            return Ok(*op == DeltaOp::Add);
        }
        let Some(checkpoint) = self.checkpoint else {
            return Ok(false);
        };
        let mut statement = self.connection.prepare(
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_cp_nodes WHERE commit_id = ?1 AND node_id = ?2)",
        )?;
        let exists: i64 = statement
            .query_row(params![checkpoint.as_bytes().as_slice(), node_id], |row| {
                row.get(0)
            })?;
        Ok(exists == 1)
    }

    /// Returns visible label ids for one Node in dictionary-id order.
    pub fn labels(&self, node_id: NodeId) -> StorageResult<Vec<LabelId>> {
        let mut labels = BTreeSet::new();
        if let Some(checkpoint) = self.checkpoint {
            let mut statement = self.connection.prepare(
                "SELECT label_id FROM main._lithograph_cp_labels WHERE commit_id = ?1 AND node_id = ?2 ORDER BY label_id",
            )?;
            let rows = statement
                .query_map(params![checkpoint.as_bytes().as_slice(), node_id], |row| {
                    row.get::<_, i64>(0)
                })?;
            for row in rows {
                let label_id = row?;
                if !self.overlay.labels.contains_key(&(node_id, label_id)) {
                    labels.insert(label_id);
                }
            }
        }
        for ((_, label_id), op) in self
            .overlay
            .labels
            .range((node_id, 0)..=(node_id, i64::MAX))
        {
            match op {
                DeltaOp::Add => {
                    labels.insert(*label_id);
                }
                DeltaOp::Remove => {
                    labels.remove(label_id);
                }
            }
        }
        Ok(labels.into_iter().collect())
    }

    /// Returns one visible property value.
    pub fn property(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: PropertyKeyId,
    ) -> StorageResult<Option<PropertyValue>> {
        if let Some(delta) = self.overlay.properties.get(&(owner_kind, owner_id, key_id)) {
            return Ok(delta.value.clone());
        }
        let cache_key = (owner_kind, owner_id, key_id);
        if let Some(value) = self.property_cache.borrow().get(&cache_key) {
            return Ok(value.clone());
        }
        let Some(checkpoint) = self.checkpoint else {
            return Ok(None);
        };
        checkpoint_property(self.connection, checkpoint, owner_kind, owner_id, key_id)
    }

    /// Returns one bounded label scan page together with the requested visible
    /// property values while reusing one checkpoint statement for the page.
    pub(crate) fn label_property_values_after(
        &self,
        label_id: LabelId,
        key_ids: &[PropertyKeyId],
        after: NodeId,
        limit: usize,
    ) -> StorageResult<ScanPage<(NodeId, Vec<Option<PropertyValue>>)>> {
        let page = self.scan_label_after(label_id, after, limit)?;
        if self.checkpoint.is_some()
            && self.overlay.nodes.is_empty()
            && self.overlay.labels.is_empty()
            && self.overlay.properties.is_empty()
        {
            return self.checkpoint_label_property_values(label_id, key_ids, after, page);
        }
        let mut checkpoint_statement = self
            .checkpoint
            .map(|_| {
                self.connection.prepare(
                    "SELECT type_tag, int_value, real_value, text_value, blob_value, aux_value \
                     FROM main._lithograph_cp_properties \
                     WHERE commit_id = ?1 AND owner_kind = ?2 AND owner_id = ?3 AND key_id = ?4",
                )
            })
            .transpose()?;
        let mut items = Vec::with_capacity(page.items.len());
        for node_id in page.items {
            let values = self.node_property_values_with_statement(
                node_id,
                key_ids,
                checkpoint_statement.as_mut(),
            )?;
            items.push((node_id, values));
        }
        Ok(ScanPage {
            items,
            next_after: page.next_after,
        })
    }

    fn checkpoint_label_property_values(
        &self,
        label_id: LabelId,
        key_ids: &[PropertyKeyId],
        after: NodeId,
        page: ScanPage<NodeId>,
    ) -> StorageResult<ScanPage<(NodeId, Vec<Option<PropertyValue>>)>> {
        let Some(checkpoint) = self.checkpoint else {
            return Err(StorageError::corrupt(
                "checkpoint label/property page requested without a checkpoint",
            ));
        };
        if page.items.is_empty() || key_ids.is_empty() {
            return Ok(ScanPage {
                items: page
                    .items
                    .into_iter()
                    .map(|node_id| (node_id, Vec::new()))
                    .collect(),
                next_after: page.next_after,
            });
        }

        let last = *page
            .items
            .last()
            .ok_or_else(|| StorageError::corrupt("checkpoint label page unexpectedly empty"))?;
        let mut values_by_key = Vec::with_capacity(key_ids.len());
        for key_id in key_ids {
            values_by_key.push(self.checkpoint_label_property_key_values(
                checkpoint, label_id, *key_id, after, last,
            )?);
        }

        let items = page
            .items
            .into_iter()
            .map(|node_id| {
                let values = values_by_key
                    .iter()
                    .map(|by_node| by_node.get(&node_id).cloned())
                    .collect();
                (node_id, values)
            })
            .collect();
        Ok(ScanPage {
            items,
            next_after: page.next_after,
        })
    }

    fn checkpoint_label_property_key_values(
        &self,
        checkpoint: HashId,
        label_id: LabelId,
        key_id: PropertyKeyId,
        after: NodeId,
        last: NodeId,
    ) -> StorageResult<BTreeMap<NodeId, PropertyValue>> {
        let mut statement = self.connection.prepare(
            "SELECT labels.node_id, properties.type_tag, properties.int_value, \
                    properties.real_value, properties.text_value, properties.blob_value, \
                    properties.aux_value \
             FROM main._lithograph_cp_labels AS labels \
             JOIN main._lithograph_cp_properties AS properties \
               ON properties.commit_id = labels.commit_id \
              AND properties.owner_kind = ?3 \
              AND properties.owner_id = labels.node_id \
              AND properties.key_id = ?4 \
             WHERE labels.commit_id = ?1 AND labels.label_id = ?2 \
               AND labels.node_id > ?5 AND labels.node_id <= ?6 \
             ORDER BY labels.node_id",
        )?;
        let rows = statement.query_map(
            params![
                checkpoint.as_bytes().as_slice(),
                label_id,
                OwnerKind::Node as i64,
                key_id,
                after,
                last,
            ],
            |row| {
                Ok((
                    row.get::<_, NodeId>(0)?,
                    PropertyColumns {
                        type_tag: row.get(1)?,
                        int_value: row.get(2)?,
                        real_value: row.get(3)?,
                        text_value: row.get(4)?,
                        blob_value: row.get(5)?,
                        aux_value: row.get(6)?,
                    },
                ))
            },
        )?;
        let mut values = BTreeMap::new();
        for row in rows {
            let (node_id, columns) = row?;
            values.insert(node_id, columns.to_value()?);
        }
        Ok(values)
    }

    fn node_property_values_with_statement(
        &self,
        node_id: NodeId,
        key_ids: &[PropertyKeyId],
        mut checkpoint_statement: Option<&mut Statement<'_>>,
    ) -> StorageResult<Vec<Option<PropertyValue>>> {
        let mut values = Vec::with_capacity(key_ids.len());
        for key_id in key_ids {
            if let Some(delta) = self
                .overlay
                .properties
                .get(&(OwnerKind::Node, node_id, *key_id))
            {
                values.push(delta.value.clone());
                continue;
            }
            values.push(self.checkpoint_property_with_statement(
                node_id,
                *key_id,
                checkpoint_statement.as_deref_mut(),
            )?);
        }
        Ok(values)
    }

    fn checkpoint_property_with_statement(
        &self,
        node_id: NodeId,
        key_id: PropertyKeyId,
        checkpoint_statement: Option<&mut Statement<'_>>,
    ) -> StorageResult<Option<PropertyValue>> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(None);
        };
        let statement = checkpoint_statement
            .ok_or_else(|| StorageError::corrupt("checkpoint property statement is unavailable"))?;
        let columns = statement
            .query_row(
                params![
                    checkpoint.as_bytes().as_slice(),
                    OwnerKind::Node as i64,
                    node_id,
                    key_id,
                ],
                property_columns_from_row,
            )
            .optional()?;
        columns.map(|columns| columns.to_value()).transpose()
    }

    /// Returns one visible Relationship by database-wide identity.
    pub fn relationship(
        &self,
        relationship_id: RelationshipId,
    ) -> StorageResult<Option<RelationshipRecord>> {
        if let Some(delta) = self.overlay.relationships.get(&relationship_id) {
            return Ok((delta.op == DeltaOp::Add).then_some(delta.record));
        }
        let Some(checkpoint) = self.checkpoint else {
            return Ok(None);
        };
        self.connection
            .query_row(
                "SELECT source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND relationship_id = ?2",
                params![checkpoint.as_bytes().as_slice(), relationship_id],
                |row| {
                    Ok(RelationshipRecord {
                        id: relationship_id,
                        source: row.get(0)?,
                        type_id: row.get(1)?,
                        target: row.get(2)?,
                    })
                },
            )
            .optional()
            .map_err(StorageError::from)
    }

    /// Returns outgoing adjacency without scanning unrelated Relationships.
    pub fn outgoing(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        self.adjacency(node_id, type_id, Direction::Outgoing)
    }

    /// Returns incoming adjacency without scanning unrelated Relationships.
    pub fn incoming(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        self.adjacency(node_id, type_id, Direction::Incoming)
    }

    /// Returns a checkpoint-independent semantic hash of the visible graph.
    pub fn semantic_hash(&self) -> StorageResult<HashId> {
        let mut hasher = blake3::Hasher::new();
        hasher.update(b"LITHOGRAPH_SNAPSHOT_V1");
        self.visit_nodes(|node_id| {
            hasher.update(b"N");
            hasher.update(&node_id.to_le_bytes());
            Ok(())
        })?;
        self.visit_labels(|node_id, label_id| {
            hasher.update(b"L");
            hasher.update(&node_id.to_le_bytes());
            hasher.update(&label_id.to_le_bytes());
            Ok(())
        })?;
        self.visit_relationships(|relationship| {
            hasher.update(b"R");
            hasher.update(&relationship.id.to_le_bytes());
            hasher.update(&relationship.source.to_le_bytes());
            hasher.update(&relationship.type_id.to_le_bytes());
            hasher.update(&relationship.target.to_le_bytes());
            Ok(())
        })?;
        self.visit_properties(|owner_kind, owner_id, key_id, value| {
            hasher.update(b"P");
            hasher.update(&(owner_kind as i64).to_le_bytes());
            hasher.update(&owner_id.to_le_bytes());
            hasher.update(&key_id.to_le_bytes());
            hasher.update(&value.canonical_bytes()?);
            Ok(())
        })?;
        Ok(HashId::from_bytes(*hasher.finalize().as_bytes()))
    }

    pub(crate) fn validate_graph_invariants(&self) -> StorageResult<()> {
        self.validate_overlay_invariants()?;
        self.validate_visible_relationships()?;
        self.validate_visible_labels()?;
        self.validate_visible_properties()
    }

    /// Validates the newly applied Layer against an already-valid parent Snapshot.
    ///
    /// Commit construction uses this bounded check so a large graph does not rescan
    /// every visible element on every write. Full-graph validation remains owned by
    /// integrity/recovery surfaces, which do not assume the parent history is valid.
    pub(crate) fn validate_overlay_invariants(&self) -> StorageResult<()> {
        self.validate_node_deltas()?;
        self.validate_label_deltas()?;
        self.validate_relationship_deltas()?;
        self.validate_property_deltas()
    }

    /// Validates one candidate Layer against this already-valid parent Snapshot.
    ///
    /// Unlike resolving the child Commit, this does not materialize the candidate
    /// Layer into all adjacency indexes. Large append batches therefore remain
    /// bounded by the candidate Layer rather than the total parent graph size.
    pub(crate) fn validate_layer_invariants(&self, layer: &LayerBuilder) -> StorageResult<()> {
        let next_node = sequence_next_id(self.connection, 1)?;
        let next_relationship = sequence_next_id(self.connection, 2)?;
        let max_persisted_relationship: i64 = self.connection.query_row(
            "SELECT coalesce(max(relationship_id), 0) FROM main._lithograph_rel_delta",
            [],
            |row| row.get(0),
        )?;

        self.validate_layer_nodes(layer, next_node)?;
        self.validate_layer_labels(layer, next_node)?;

        self.validate_layer_relationships(
            layer,
            next_node,
            next_relationship,
            max_persisted_relationship,
        )?;

        self.validate_layer_properties(layer, next_node, next_relationship)?;

        self.validate_removed_nodes(layer)
    }

    fn validate_layer_nodes(&self, layer: &LayerBuilder, next_node: i64) -> StorageResult<()> {
        for node_id in layer.nodes.keys() {
            require_allocated_below(*node_id, next_node, "NodeId")?;
        }
        Ok(())
    }

    fn validate_layer_labels(&self, layer: &LayerBuilder, next_node: i64) -> StorageResult<()> {
        let label_ids = layer
            .labels
            .keys()
            .map(|(_, label)| *label)
            .collect::<BTreeSet<_>>();
        for label_id in label_ids {
            require_dictionary_id(self.connection, DictionaryKind::Label, label_id)?;
        }
        for ((node_id, label_id), op) in &layer.labels {
            require_allocated_below(*node_id, next_node, "NodeId")?;
            if *op == DeltaOp::Add && !self.node_exists_after(layer, *node_id)? {
                return Err(StorageError::corrupt(format!(
                    "LabelId {label_id} refers to missing NodeId {node_id}"
                )));
            }
        }
        Ok(())
    }

    fn validate_layer_relationships(
        &self,
        layer: &LayerBuilder,
        next_node: i64,
        next_relationship: i64,
        max_persisted_relationship: i64,
    ) -> StorageResult<()> {
        let type_ids = layer
            .relationships
            .values()
            .map(|delta| delta.record.type_id)
            .collect::<BTreeSet<_>>();
        for type_id in type_ids {
            require_dictionary_id(self.connection, DictionaryKind::RelationshipType, type_id)?;
        }
        for delta in layer.relationships.values() {
            self.validate_layer_relationship(
                layer,
                delta,
                next_node,
                next_relationship,
                max_persisted_relationship,
            )?;
        }
        Ok(())
    }

    fn validate_layer_relationship(
        &self,
        layer: &LayerBuilder,
        delta: &super::layer::RelationshipDelta,
        next_node: i64,
        next_relationship: i64,
        max_persisted_relationship: i64,
    ) -> StorageResult<()> {
        let record = delta.record;
        require_allocated_below(record.id, next_relationship, "RelationshipId")?;
        require_allocated_below(record.source, next_node, "source NodeId")?;
        require_allocated_below(record.target, next_node, "target NodeId")?;
        if record.id <= max_persisted_relationship {
            require_relationship_tuple_stable(self.connection, record)?;
        }
        if delta.op == DeltaOp::Add {
            if !self.node_exists_after(layer, record.source)?
                || !self.node_exists_after(layer, record.target)?
            {
                return Err(StorageError::corrupt(format!(
                    "RelationshipId {} has a missing endpoint",
                    record.id
                )));
            }
        } else if self.relationship_has_visible_property_after(layer, record.id)? {
            return Err(StorageError::corrupt(format!(
                "RelationshipId {} is removed while visible Properties still reference it",
                record.id
            )));
        }
        Ok(())
    }

    fn validate_layer_properties(
        &self,
        layer: &LayerBuilder,
        next_node: i64,
        next_relationship: i64,
    ) -> StorageResult<()> {
        let property_keys = layer
            .properties
            .keys()
            .map(|(_, _, key_id)| *key_id)
            .collect::<BTreeSet<_>>();
        for key_id in property_keys {
            require_dictionary_id(self.connection, DictionaryKind::PropertyKey, key_id)?;
        }
        for ((owner_kind, owner_id, key_id), delta) in &layer.properties {
            self.validate_layer_property(
                layer,
                *owner_kind,
                *owner_id,
                *key_id,
                delta,
                next_node,
                next_relationship,
            )?;
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "property invariant validation keeps the owner slot and allocation bounds explicit"
    )]
    fn validate_layer_property(
        &self,
        layer: &LayerBuilder,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: i64,
        delta: &super::layer::PropertyDelta,
        next_node: i64,
        next_relationship: i64,
    ) -> StorageResult<()> {
        match owner_kind {
            OwnerKind::Node => require_allocated_below(owner_id, next_node, "NodeId")?,
            OwnerKind::Relationship => {
                require_allocated_below(owner_id, next_relationship, "RelationshipId")?
            }
        }
        if delta.op != DeltaOp::Add {
            return Ok(());
        }
        let owner_exists = match owner_kind {
            OwnerKind::Node => self.node_exists_after(layer, owner_id)?,
            OwnerKind::Relationship => layer.relationships.get(&owner_id).map_or_else(
                || self.relationship(owner_id).map(|value| value.is_some()),
                |relationship| Ok(relationship.op == DeltaOp::Add),
            )?,
        };
        if !owner_exists {
            return Err(StorageError::corrupt(format!(
                "property key {key_id} refers to missing owner {owner_id}"
            )));
        }
        Ok(())
    }

    fn validate_removed_nodes(&self, layer: &LayerBuilder) -> StorageResult<()> {
        for (node_id, op) in &layer.nodes {
            if *op == DeltaOp::Remove && self.node_has_visible_dependents_after(layer, *node_id)? {
                return Err(StorageError::corrupt(format!(
                    "NodeId {node_id} is removed while visible graph state still references it"
                )));
            }
        }
        Ok(())
    }

    fn node_exists_after(&self, layer: &LayerBuilder, node_id: NodeId) -> StorageResult<bool> {
        match layer.nodes.get(&node_id) {
            Some(DeltaOp::Add) => Ok(true),
            Some(DeltaOp::Remove) => Ok(false),
            None => self.node_exists(node_id),
        }
    }

    fn node_has_visible_dependents_after(
        &self,
        layer: &LayerBuilder,
        node_id: NodeId,
    ) -> StorageResult<bool> {
        if self.node_has_relationship_dependents_after(layer, node_id)? {
            return Ok(true);
        }
        if self.node_has_label_dependents_after(layer, node_id)? {
            return Ok(true);
        }
        self.node_has_property_dependents_after(layer, node_id)
    }

    fn node_has_relationship_dependents_after(
        &self,
        layer: &LayerBuilder,
        node_id: NodeId,
    ) -> StorageResult<bool> {
        let relationship_survives = self
            .outgoing(node_id, None)?
            .into_iter()
            .chain(self.incoming(node_id, None)?)
            .any(|record| {
                !matches!(
                    layer.relationships.get(&record.id),
                    Some(delta) if delta.op == DeltaOp::Remove
                )
            });
        if relationship_survives
            || layer.relationships.values().any(|delta| {
                delta.op == DeltaOp::Add
                    && (delta.record.source == node_id || delta.record.target == node_id)
            })
        {
            return Ok(true);
        }
        Ok(false)
    }

    fn node_has_label_dependents_after(
        &self,
        layer: &LayerBuilder,
        node_id: NodeId,
    ) -> StorageResult<bool> {
        let label_survives = self.labels(node_id)?.into_iter().any(|label_id| {
            !matches!(
                layer.labels.get(&(node_id, label_id)),
                Some(DeltaOp::Remove)
            )
        });
        if label_survives
            || layer
                .labels
                .iter()
                .any(|((owner, _), op)| *owner == node_id && *op == DeltaOp::Add)
        {
            return Ok(true);
        }
        Ok(false)
    }

    fn node_has_property_dependents_after(
        &self,
        layer: &LayerBuilder,
        node_id: NodeId,
    ) -> StorageResult<bool> {
        self.owner_has_visible_property_after(layer, OwnerKind::Node, node_id)
    }

    fn relationship_has_visible_property_after(
        &self,
        layer: &LayerBuilder,
        relationship_id: RelationshipId,
    ) -> StorageResult<bool> {
        self.owner_has_visible_property_after(layer, OwnerKind::Relationship, relationship_id)
    }

    fn owner_has_visible_property_after(
        &self,
        layer: &LayerBuilder,
        owner_kind: OwnerKind,
        owner_id: i64,
    ) -> StorageResult<bool> {
        let property_survives = self
            .properties(owner_kind, owner_id)?
            .iter()
            .any(|(key_id, _)| {
                !matches!(
                    layer.properties.get(&(owner_kind, owner_id, *key_id)),
                    Some(delta) if delta.op == DeltaOp::Remove
                )
            });
        Ok(property_survives
            || layer
                .properties
                .iter()
                .any(|((slot_kind, owner, _), delta)| {
                    *slot_kind == owner_kind && *owner == owner_id && delta.op == DeltaOp::Add
                }))
    }

    fn validate_visible_relationships(&self) -> StorageResult<()> {
        self.visit_relationships(|relationship| {
            if !self.node_exists(relationship.source)? || !self.node_exists(relationship.target)? {
                return Err(StorageError::corrupt(format!(
                    "RelationshipId {} has a missing endpoint",
                    relationship.id
                )));
            }
            Ok(())
        })
    }

    fn validate_visible_labels(&self) -> StorageResult<()> {
        self.visit_labels(|node_id, label_id| {
            if !self.node_exists(node_id)? {
                return Err(StorageError::corrupt(format!(
                    "LabelId {label_id} refers to missing NodeId {node_id}"
                )));
            }
            Ok(())
        })
    }

    fn validate_visible_properties(&self) -> StorageResult<()> {
        self.visit_properties(|owner_kind, owner_id, key_id, _| {
            let owner_exists = match owner_kind {
                OwnerKind::Node => self.node_exists(owner_id)?,
                OwnerKind::Relationship => self.relationship(owner_id)?.is_some(),
            };
            if !owner_exists {
                return Err(StorageError::corrupt(format!(
                    "PropertyKeyId {key_id} refers to missing owner {owner_id}"
                )));
            }
            Ok(())
        })
    }

    fn validate_node_deltas(&self) -> StorageResult<()> {
        for (node_id, op) in &self.overlay.nodes {
            require_allocated(self.connection, 1, *node_id, "NodeId")?;
            match op {
                DeltaOp::Add => {}
                DeltaOp::Remove => {
                    if !self.outgoing(*node_id, None)?.is_empty()
                        || !self.incoming(*node_id, None)?.is_empty()
                        || !self.labels(*node_id)?.is_empty()
                        || !self.properties(OwnerKind::Node, *node_id)?.is_empty()
                    {
                        return Err(StorageError::corrupt(format!(
                            "NodeId {node_id} is removed while visible graph state still references it"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_label_deltas(&self) -> StorageResult<()> {
        for ((node_id, label_id), op) in &self.overlay.labels {
            require_allocated(self.connection, 1, *node_id, "NodeId")?;
            require_dictionary_id(self.connection, DictionaryKind::Label, *label_id)?;
            if *op == DeltaOp::Add && !self.node_exists(*node_id)? {
                return Err(StorageError::corrupt(format!(
                    "LabelId {label_id} refers to missing NodeId {node_id}"
                )));
            }
        }
        Ok(())
    }

    fn validate_relationship_deltas(&self) -> StorageResult<()> {
        for delta in self.overlay.relationships.values() {
            require_allocated(self.connection, 2, delta.record.id, "RelationshipId")?;
            require_allocated(self.connection, 1, delta.record.source, "source NodeId")?;
            require_allocated(self.connection, 1, delta.record.target, "target NodeId")?;
            require_dictionary_id(
                self.connection,
                DictionaryKind::RelationshipType,
                delta.record.type_id,
            )?;
            require_relationship_tuple_stable(self.connection, delta.record)?;
            if delta.op == DeltaOp::Add
                && (!self.node_exists(delta.record.source)?
                    || !self.node_exists(delta.record.target)?)
            {
                return Err(StorageError::corrupt(format!(
                    "RelationshipId {} has a missing endpoint",
                    delta.record.id
                )));
            }
            if delta.op == DeltaOp::Remove
                && !self
                    .properties(OwnerKind::Relationship, delta.record.id)?
                    .is_empty()
            {
                return Err(StorageError::corrupt(format!(
                    "RelationshipId {} is removed while visible Properties still reference it",
                    delta.record.id
                )));
            }
        }
        Ok(())
    }

    fn validate_property_deltas(&self) -> StorageResult<()> {
        for ((owner_kind, owner_id, key_id), delta) in &self.overlay.properties {
            require_dictionary_id(self.connection, DictionaryKind::PropertyKey, *key_id)?;
            match owner_kind {
                OwnerKind::Node => require_allocated(self.connection, 1, *owner_id, "NodeId")?,
                OwnerKind::Relationship => {
                    require_allocated(self.connection, 2, *owner_id, "RelationshipId")?
                }
            }
            if delta.op != DeltaOp::Add {
                continue;
            }
            let owner_exists = match owner_kind {
                OwnerKind::Node => self.node_exists(*owner_id)?,
                OwnerKind::Relationship => self.relationship(*owner_id)?.is_some(),
            };
            if !owner_exists {
                return Err(StorageError::corrupt(format!(
                    "property key {key_id} refers to missing owner {owner_id}"
                )));
            }
        }
        Ok(())
    }

    fn adjacency(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        direction: Direction,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        let mut records = self.base_adjacency(node_id, type_id, direction)?;
        records.retain(|record| !self.overlay.relationships.contains_key(&record.id));
        let index = match direction {
            Direction::Outgoing => &self.overlay.outgoing,
            Direction::Incoming => &self.overlay.incoming,
        };
        let min_type = type_id.unwrap_or(0);
        let max_type = type_id.unwrap_or(i64::MAX);
        for (_, delta) in
            index.range((node_id, min_type, 0, 0)..=(node_id, max_type, i64::MAX, i64::MAX))
        {
            if delta.op == DeltaOp::Add {
                records.push(delta.record);
            }
        }
        records.sort_by_key(|record| adjacency_sort_key(*record, direction));
        Ok(records)
    }

    fn base_adjacency(
        &self,
        node_id: NodeId,
        type_id: Option<RelationshipTypeId>,
        direction: Direction,
    ) -> StorageResult<Vec<RelationshipRecord>> {
        let Some(checkpoint) = self.checkpoint else {
            return Ok(Vec::new());
        };
        match type_id {
            Some(type_id) => {
                query_base_adjacency_typed(self.connection, checkpoint, node_id, type_id, direction)
            }
            None => query_base_adjacency_all(self.connection, checkpoint, node_id, direction),
        }
    }
}

fn merge_query_local_layer(target: &mut LayerBuilder, source: &LayerBuilder) {
    target.nodes.extend(source.nodes.clone());
    target.labels.extend(source.labels.clone());
    target.relationships.extend(source.relationships.clone());
    target.properties.extend(source.properties.clone());
}

fn query_local_cache_identity(
    commit: HashId,
    layer: &LayerBuilder,
    schema: Option<&SchemaState>,
) -> StorageResult<HashId> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_QUERY_LOCAL_SNAPSHOT_V1");
    hasher.update(commit.as_bytes());
    hasher.update(layer.content_hash()?.as_bytes());
    if let Some(schema) = schema {
        hasher.update(&schema.canonical_blob()?);
    }
    Ok(HashId::from_bytes(*hasher.finalize().as_bytes()))
}

fn advance_query_local_cache_identity(previous: HashId, layer_hash: HashId) -> HashId {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_QUERY_LOCAL_SNAPSHOT_STEP_V1");
    hasher.update(previous.as_bytes());
    hasher.update(layer_hash.as_bytes());
    HashId::from_bytes(*hasher.finalize().as_bytes())
}

#[derive(Clone, Copy)]
enum Direction {
    Outgoing,
    Incoming,
}

#[derive(Clone, Copy)]
enum DictionaryKind {
    Label,
    RelationshipType,
    PropertyKey,
}

fn require_allocated(
    connection: &Connection,
    sequence_kind: i64,
    id: i64,
    name: &str,
) -> StorageResult<()> {
    let next_id: i64 = connection.query_row(
        "SELECT next_id FROM main._lithograph_sequences WHERE kind = ?1",
        [sequence_kind],
        |row| row.get(0),
    )?;
    if id > 0 && id < next_id {
        Ok(())
    } else {
        Err(StorageError::corrupt(format!(
            "{name} {id} was not allocated by the database sequence"
        )))
    }
}

fn sequence_next_id(connection: &Connection, kind: i64) -> StorageResult<i64> {
    connection
        .query_row(
            "SELECT next_id FROM main._lithograph_sequences WHERE kind = ?1",
            [kind],
            |row| row.get(0),
        )
        .map_err(StorageError::from)
}

fn require_allocated_below(id: i64, next_id: i64, name: &str) -> StorageResult<()> {
    if id > 0 && id < next_id {
        Ok(())
    } else {
        Err(StorageError::corrupt(format!(
            "{name} {id} was not allocated by the database sequence"
        )))
    }
}

fn require_dictionary_id(
    connection: &Connection,
    kind: DictionaryKind,
    id: i64,
) -> StorageResult<()> {
    let sql = match kind {
        DictionaryKind::Label => {
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_labels WHERE id = ?1)"
        }
        DictionaryKind::RelationshipType => {
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_rel_types WHERE id = ?1)"
        }
        DictionaryKind::PropertyKey => {
            "SELECT EXISTS(SELECT 1 FROM main._lithograph_prop_keys WHERE id = ?1)"
        }
    };
    let exists: i64 = connection.query_row(sql, [id], |row| row.get(0))?;
    if exists == 1 {
        Ok(())
    } else {
        Err(StorageError::corrupt(format!(
            "dictionary id {id} does not exist"
        )))
    }
}

fn require_relationship_tuple_stable(
    connection: &Connection,
    record: RelationshipRecord,
) -> StorageResult<()> {
    let changed: i64 = connection.query_row(
        "SELECT EXISTS(\
            SELECT 1 FROM main._lithograph_rel_delta INDEXED BY _lithograph_rel_delta_identity \
            WHERE relationship_id = ?1 \
              AND (source_id != ?2 OR type_id != ?3 OR target_id != ?4)\
        )",
        params![record.id, record.source, record.type_id, record.target],
        |row| row.get(0),
    )?;
    if changed == 0 {
        Ok(())
    } else {
        Err(StorageError::corrupt(format!(
            "RelationshipId {} changed its immutable endpoints or type",
            record.id
        )))
    }
}

fn adjacency_sort_key(record: RelationshipRecord, direction: Direction) -> (i64, i64, i64) {
    match direction {
        Direction::Outgoing => (record.type_id, record.target, record.id),
        Direction::Incoming => (record.type_id, record.source, record.id),
    }
}

fn query_base_adjacency_typed(
    connection: &Connection,
    checkpoint: HashId,
    node_id: NodeId,
    type_id: RelationshipTypeId,
    direction: Direction,
) -> StorageResult<Vec<RelationshipRecord>> {
    let sql = adjacency_sql(direction, true);
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(
        params![checkpoint.as_bytes().as_slice(), node_id, type_id],
        relationship_from_checkpoint_row,
    )?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from)
}

fn query_base_adjacency_all(
    connection: &Connection,
    checkpoint: HashId,
    node_id: NodeId,
    direction: Direction,
) -> StorageResult<Vec<RelationshipRecord>> {
    let sql = adjacency_sql(direction, false);
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(
        params![checkpoint.as_bytes().as_slice(), node_id],
        relationship_from_checkpoint_row,
    )?;
    rows.collect::<Result<Vec<_>, _>>()
        .map_err(StorageError::from)
}

fn adjacency_sql(direction: Direction, typed: bool) -> &'static str {
    match (direction, typed) {
        (Direction::Outgoing, true) => {
            "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND source_id = ?2 AND type_id = ?3 ORDER BY type_id, target_id, relationship_id"
        }
        (Direction::Outgoing, false) => {
            "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND source_id = ?2 ORDER BY type_id, target_id, relationship_id"
        }
        (Direction::Incoming, true) => {
            "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND target_id = ?2 AND type_id = ?3 ORDER BY type_id, source_id, relationship_id"
        }
        (Direction::Incoming, false) => {
            "SELECT relationship_id, source_id, type_id, target_id FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND target_id = ?2 ORDER BY type_id, source_id, relationship_id"
        }
    }
}

fn relationship_from_checkpoint_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<RelationshipRecord> {
    Ok(RelationshipRecord {
        id: row.get(0)?,
        source: row.get(1)?,
        type_id: row.get(2)?,
        target: row.get(3)?,
    })
}

fn checkpoint_property(
    connection: &Connection,
    checkpoint: HashId,
    owner_kind: OwnerKind,
    owner_id: i64,
    key_id: PropertyKeyId,
) -> StorageResult<Option<PropertyValue>> {
    let mut statement = connection.prepare(
        "SELECT type_tag, int_value, real_value, text_value, blob_value, aux_value FROM main._lithograph_cp_properties WHERE commit_id = ?1 AND owner_kind = ?2 AND owner_id = ?3 AND key_id = ?4",
    )?;
    let columns = statement
        .query_row(
            params![
                checkpoint.as_bytes().as_slice(),
                owner_kind as i64,
                owner_id,
                key_id
            ],
            property_columns_from_row,
        )
        .optional()?;
    columns.map(|columns| columns.to_value()).transpose()
}

fn property_columns_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<PropertyColumns> {
    Ok(PropertyColumns {
        type_tag: row.get(0)?,
        int_value: row.get(1)?,
        real_value: row.get(2)?,
        text_value: row.get(3)?,
        blob_value: row.get(4)?,
        aux_value: row.get(5)?,
    })
}

impl Overlay {
    fn apply(&mut self, layer: LayerBuilder) {
        self.nodes.extend(layer.nodes);
        for ((node_id, label_id), op) in layer.labels {
            self.labels.insert((node_id, label_id), op);
            self.labels_by_label.insert((label_id, node_id), op);
        }
        for (relationship_id, delta) in layer.relationships {
            self.apply_relationship_delta(relationship_id, delta);
        }
        self.properties.extend(layer.properties);
    }

    fn apply_persisted_layer(
        &mut self,
        connection: &Connection,
        layer_id: i64,
    ) -> StorageResult<()> {
        visit_node_deltas(connection, layer_id, |node_id, op| {
            self.nodes.insert(node_id, op);
            Ok(())
        })?;
        visit_label_deltas(connection, layer_id, |node_id, label_id, op| {
            self.labels.insert((node_id, label_id), op);
            self.labels_by_label.insert((label_id, node_id), op);
            Ok(())
        })?;
        visit_relationship_deltas(connection, layer_id, |record, op| {
            self.apply_relationship_delta(record.id, RelationshipDelta { op, record });
            Ok(())
        })?;
        visit_property_deltas(
            connection,
            layer_id,
            |owner_kind, owner_id, key_id, delta| {
                self.properties
                    .insert((owner_kind, owner_id, key_id), delta);
                Ok(())
            },
        )?;
        Ok(())
    }

    fn apply_relationship_delta(&mut self, relationship_id: i64, delta: RelationshipDelta) {
        if let Some(previous) = self.relationships.get(&relationship_id) {
            self.outgoing.remove(&outgoing_key(previous.record));
            self.incoming.remove(&incoming_key(previous.record));
            self.outgoing_by_id
                .remove(&(previous.record.source, relationship_id));
            self.incoming_by_id
                .remove(&(previous.record.target, relationship_id));
            self.incident_by_id
                .remove(&(previous.record.source, relationship_id));
            self.incident_by_id
                .remove(&(previous.record.target, relationship_id));
        }
        self.outgoing.insert(outgoing_key(delta.record), delta);
        self.incoming.insert(incoming_key(delta.record), delta);
        self.outgoing_by_id
            .insert((delta.record.source, relationship_id), delta);
        self.incoming_by_id
            .insert((delta.record.target, relationship_id), delta);
        self.incident_by_id
            .insert((delta.record.source, relationship_id), delta);
        self.incident_by_id
            .insert((delta.record.target, relationship_id), delta);
        self.relationships.insert(relationship_id, delta);
    }
}

fn outgoing_key(record: RelationshipRecord) -> (i64, i64, i64, i64) {
    (record.source, record.type_id, record.target, record.id)
}

fn incoming_key(record: RelationshipRecord) -> (i64, i64, i64, i64) {
    (record.target, record.type_id, record.source, record.id)
}
