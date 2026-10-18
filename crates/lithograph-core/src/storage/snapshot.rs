//! Snapshot resolution over a checkpoint plus first-parent Layer overlay.

use std::cell::RefCell;
use std::collections::{BTreeMap, BTreeSet};
use std::rc::Rc;
use std::sync::Arc;

use rusqlite::{Connection, OptionalExtension, params};

use super::layer::{DeltaOp, LayerBuilder, PropertyDelta, RelationshipDelta, RelationshipRecord};
use super::snapshot_lineage::resolve_lineage;
use super::snapshot_read::{self, ReadLease, SnapshotReadObserver};
use super::{
    HashId, LabelId, NodeId, OwnerKind, PropertyKeyId, PropertyValue, RelationshipId,
    RelationshipTypeId, SchemaState, StorageError, StorageResult,
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
    pub(super) property_memory: BTreeMap<(OwnerKind, i64, PropertyKeyId), ReadLease>,
    pub(super) structure_memory: Vec<ReadLease>,
}

/// Connection-independent graph state resolved once for one query execution.
#[derive(Debug, Clone)]
pub(crate) struct ResolvedSnapshotState {
    pub(super) commit: HashId,
    pub(super) cache_identity: HashId,
    pub(super) checkpoint: Option<HashId>,
    pub(super) overlay: Rc<Overlay>,
    pub(super) property_cache: Rc<RefCell<PropertyCache>>,
    pub(super) query_local_layer: Option<Arc<LayerBuilder>>,
    pub(super) schema_override: Option<Arc<SchemaState>>,
    pub(super) read_observer: Option<Rc<dyn SnapshotReadObserver>>,
    pub(super) local_memory: Option<Rc<super::snapshot_staged::LocalLayerMemory>>,
    pub(super) schema_memory: Option<ReadLease>,
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
    pub(super) state: ResolvedSnapshotState,
}

#[derive(Debug)]
pub(super) struct CachedProperty {
    pub(super) value: Option<PropertyValue>,
    pub(super) _memory: Option<ReadLease>,
}

pub(super) type PropertyCache = BTreeMap<(OwnerKind, i64, PropertyKeyId), CachedProperty>;

impl<'connection> Snapshot<'connection> {
    /// Resolves a Commit-pinned snapshot.
    pub fn resolve(connection: &'connection Connection, commit: HashId) -> StorageResult<Self> {
        let state =
            Self::resolve_state_with_checkpoint_skip(connection, commit, None, None, &|| Ok(()))?;
        Ok(Self::from_resolved_state(connection, &state))
    }

    pub(crate) fn from_resolved_state(
        connection: &'connection Connection,
        state: &ResolvedSnapshotState,
    ) -> Self {
        Self {
            connection,
            state: state.clone(),
        }
    }

    pub(crate) fn resolved_state(&self) -> ResolvedSnapshotState {
        self.state.clone()
    }

    #[cfg(test)]
    pub(crate) fn resolve_with_layer_and_schema(
        connection: &'connection Connection,
        commit: HashId,
        layer: &LayerBuilder,
        schema: SchemaState,
    ) -> StorageResult<Self> {
        let mut snapshot = Self::resolve(connection, commit)?;
        Rc::make_mut(&mut snapshot.state.overlay).apply(layer.clone());
        snapshot.state.query_local_layer = Some(Arc::new(layer.clone()));
        snapshot.state.cache_identity =
            query_local_cache_identity_checked(commit, layer, Some(&schema), &|| Ok(()))?;
        snapshot.state.schema_override = Some(Arc::new(schema));
        Ok(snapshot)
    }

    pub(super) fn resolve_state_with_checkpoint_skip(
        connection: &Connection,
        commit: HashId,
        skip_checkpoint: Option<HashId>,
        read_observer: Option<Rc<dyn SnapshotReadObserver>>,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<ResolvedSnapshotState> {
        let (checkpoint, layer_ids, _lineage_memory) = resolve_lineage(
            connection,
            commit,
            skip_checkpoint,
            read_observer.as_deref(),
            check,
        )?;
        #[cfg(feature = "test-support")]
        crate::performance::record_resolved_state(layer_ids.len());
        let mut overlay = Overlay::default();
        for layer_id in layer_ids.into_iter().rev() {
            check()?;
            overlay.apply_persisted_layer_observed(
                connection,
                layer_id,
                read_observer.as_deref(),
                check,
            )?;
        }
        Ok(ResolvedSnapshotState {
            commit,
            cache_identity: commit,
            checkpoint,
            overlay: Rc::new(overlay),
            property_cache: Rc::new(RefCell::new(BTreeMap::new())),
            query_local_layer: None,
            schema_override: None,
            read_observer,
            local_memory: None,
            schema_memory: None,
        })
    }

    /// Commit pinned by this snapshot.
    pub fn commit(&self) -> HashId {
        self.state.commit
    }

    pub(crate) fn cache_identity(&self) -> HashId {
        self.state.cache_identity
    }

    pub(crate) fn query_local_layer(&self) -> Option<&LayerBuilder> {
        self.state.query_local_layer.as_deref()
    }

    pub(crate) fn schema_state(&self) -> StorageResult<SchemaState> {
        match &self.state.schema_override {
            Some(schema) => {
                let check = || self.read_checkpoint();
                let _memory = snapshot_read::reserve(
                    self.state.read_observer.as_deref(),
                    super::schema_state_memory_bytes(schema, &check)?,
                )?;
                schema.clone_checked(&check)
            }
            None => SchemaState::load(self.connection, self.state.commit),
        }
    }

    pub(crate) fn query_schema_state(&self) -> Option<&SchemaState> {
        self.state.schema_override.as_deref()
    }

    pub(crate) fn connection_for_query(&self) -> &'connection Connection {
        self.connection
    }

    /// Returns whether a Node is visible in this snapshot.
    pub fn node_exists(&self, node_id: NodeId) -> StorageResult<bool> {
        if let Some(op) = self.state.overlay.nodes.get(&node_id) {
            return Ok(*op == DeltaOp::Add);
        }
        let Some(checkpoint) = self.state.checkpoint else {
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
        if let Some(checkpoint) = self.state.checkpoint {
            let mut statement = self.connection.prepare(
                "SELECT label_id FROM main._lithograph_cp_labels WHERE commit_id = ?1 AND node_id = ?2 ORDER BY label_id",
            )?;
            let rows = statement
                .query_map(params![checkpoint.as_bytes().as_slice(), node_id], |row| {
                    row.get::<_, i64>(0)
                })?;
            for row in rows {
                let label_id = row?;
                if !self.state.overlay.labels.contains_key(&(node_id, label_id)) {
                    labels.insert(label_id);
                }
            }
        }
        for ((_, label_id), op) in self
            .state
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
        self.with_property(owner_kind, owner_id, key_id, |value| {
            let _memory = value
                .map(|value| {
                    snapshot_read::reserve_value(
                        value,
                        self.state.read_observer.as_deref(),
                        &|| self.read_checkpoint(),
                    )
                })
                .transpose()?;
            value
                .map(|value| snapshot_read::clone_value_checked(value, &|| self.read_checkpoint()))
                .transpose()
        })
    }

    /// Returns one visible Relationship by database-wide identity.
    pub fn relationship(
        &self,
        relationship_id: RelationshipId,
    ) -> StorageResult<Option<RelationshipRecord>> {
        if let Some(delta) = self.state.overlay.relationships.get(&relationship_id) {
            return Ok((delta.op == DeltaOp::Add).then_some(delta.record));
        }
        let Some(checkpoint) = self.state.checkpoint else {
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
        self.validate_graph_invariants_checked(&|| Ok(()))
    }

    pub(crate) fn validate_graph_invariants_checked(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        check()?;
        self.validate_overlay_invariants_checked(check)?;
        self.validate_visible_relationships(check)?;
        self.validate_visible_labels(check)?;
        self.validate_visible_properties(check)?;
        check()
    }

    /// Validates the newly applied Layer against an already-valid parent Snapshot.
    ///
    /// Commit construction uses this bounded check so a large graph does not rescan
    /// every visible element on every write. Full-graph validation remains owned by
    /// integrity/recovery surfaces, which do not assume the parent history is valid.
    fn validate_overlay_invariants_checked(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.validate_node_deltas(check)?;
        self.validate_label_deltas(check)?;
        self.validate_relationship_deltas(check)?;
        self.validate_property_deltas(check)
    }

    /// Validates one candidate Layer against this already-valid parent Snapshot.
    ///
    /// Unlike resolving the child Commit, this does not materialize the candidate
    /// Layer into all adjacency indexes. Large append batches therefore remain
    /// bounded by the candidate Layer rather than the total parent graph size.
    pub(crate) fn validate_layer_invariants_checked(
        &self,
        layer: &LayerBuilder,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        check()?;
        let next_node = sequence_next_id(self.connection, 1)?;
        let next_relationship = sequence_next_id(self.connection, 2)?;
        let max_persisted_relationship: i64 = self.connection.query_row(
            "SELECT coalesce(max(relationship_id), 0) FROM main._lithograph_rel_delta",
            [],
            |row| row.get(0),
        )?;
        self.validate_layer_nodes(layer, next_node, check)?;
        self.validate_layer_labels(layer, next_node, check)?;
        self.validate_layer_relationships(
            layer,
            next_node,
            next_relationship,
            max_persisted_relationship,
            check,
        )?;
        self.validate_layer_properties(layer, next_node, next_relationship, check)?;
        self.validate_removed_nodes(layer, check)?;
        check()
    }

    fn validate_layer_nodes(
        &self,
        layer: &LayerBuilder,
        next_node: i64,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        for node_id in layer.nodes.keys() {
            check()?;
            require_allocated_below(*node_id, next_node, "NodeId")?;
        }
        Ok(())
    }

    fn validate_dictionary_ids(
        &self,
        ids: impl Iterator<Item = i64>,
        kind: DictionaryKind,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        let mut unique = BTreeSet::new();
        let mut memory = Vec::new();
        for id in ids {
            check()?;
            if !unique.contains(&id) {
                if let Some(lease) =
                    snapshot_read::reserve(self.state.read_observer.as_deref(), 256)?
                {
                    memory.push(lease);
                }
                unique.insert(id);
            }
        }
        for id in unique {
            check()?;
            require_dictionary_id(self.connection, kind, id)?;
        }
        Ok(())
    }

    fn validate_layer_labels(
        &self,
        layer: &LayerBuilder,
        next_node: i64,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.validate_dictionary_ids(
            layer.labels.keys().map(|(_, label)| *label),
            DictionaryKind::Label,
            check,
        )?;
        for ((node_id, label_id), op) in &layer.labels {
            check()?;
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
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.validate_dictionary_ids(
            layer
                .relationships
                .values()
                .map(|delta| delta.record.type_id),
            DictionaryKind::RelationshipType,
            check,
        )?;
        for delta in layer.relationships.values() {
            check()?;
            self.validate_layer_relationship(
                layer,
                delta,
                next_node,
                next_relationship,
                max_persisted_relationship,
                check,
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
        check: &dyn Fn() -> StorageResult<()>,
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
        } else if self.owner_has_visible_property_after(
            layer,
            OwnerKind::Relationship,
            record.id,
            check,
        )? {
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
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.validate_dictionary_ids(
            layer.properties.keys().map(|(_, _, key_id)| *key_id),
            DictionaryKind::PropertyKey,
            check,
        )?;
        for ((owner_kind, owner_id, key_id), delta) in &layer.properties {
            check()?;
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

    fn validate_removed_nodes(
        &self,
        layer: &LayerBuilder,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        for (node_id, op) in &layer.nodes {
            check()?;
            if *op == DeltaOp::Remove
                && self.node_has_visible_dependents_after(layer, *node_id, check)?
            {
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
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<bool> {
        if self.node_has_relationship_dependents_after(layer, node_id, check)? {
            return Ok(true);
        }
        if self.node_has_label_dependents_after(layer, node_id, check)? {
            return Ok(true);
        }
        self.owner_has_visible_property_after(layer, OwnerKind::Node, node_id, check)
    }

    fn node_has_relationship_dependents_after(
        &self,
        layer: &LayerBuilder,
        node_id: NodeId,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<bool> {
        let mut cursor = None;
        loop {
            check()?;
            let page = self.scan_incident_page(node_id, None, cursor, 256)?;
            for record in page.items {
                check()?;
                if !matches!(layer.relationships.get(&record.id), Some(delta) if delta.op == DeltaOp::Remove)
                {
                    return Ok(true);
                }
            }
            let Some(next) = page.next_cursor else { break };
            cursor = Some(next);
        }
        for delta in layer.relationships.values() {
            check()?;
            if delta.op == DeltaOp::Add
                && (delta.record.source == node_id || delta.record.target == node_id)
            {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn node_has_label_dependents_after(
        &self,
        layer: &LayerBuilder,
        node_id: NodeId,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<bool> {
        let mut survives = false;
        self.visit_node_labels_checked(node_id, check, |label_id| {
            survives |= !matches!(
                layer.labels.get(&(node_id, label_id)),
                Some(DeltaOp::Remove)
            );
            Ok::<_, StorageError>(())
        })?;
        if survives {
            return Ok(true);
        }
        for ((owner, _), op) in &layer.labels {
            check()?;
            if *owner == node_id && *op == DeltaOp::Add {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn owner_has_visible_property_after(
        &self,
        layer: &LayerBuilder,
        owner_kind: OwnerKind,
        owner_id: i64,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<bool> {
        let mut survives = false;
        self.visit_owner_properties_checked(owner_kind, owner_id, check, |key_id, _| {
            survives |= !matches!(layer.properties.get(&(owner_kind, owner_id, key_id)), Some(delta) if delta.op == DeltaOp::Remove);
            Ok::<_, StorageError>(())
        })?;
        if survives {
            return Ok(true);
        }
        for ((slot_kind, owner, _), delta) in &layer.properties {
            check()?;
            if *slot_kind == owner_kind && *owner == owner_id && delta.op == DeltaOp::Add {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn validate_visible_relationships(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.visit_relationships_checked(check, |relationship| {
            check()?;
            self.require_visible_relationship_endpoints(relationship)
        })
    }

    fn require_visible_relationship_endpoints(
        &self,
        record: RelationshipRecord,
    ) -> StorageResult<()> {
        if !self.node_exists(record.source)? || !self.node_exists(record.target)? {
            return Err(StorageError::corrupt(format!(
                "RelationshipId {} has a missing endpoint",
                record.id
            )));
        }
        Ok(())
    }

    fn validate_visible_labels(&self, check: &dyn Fn() -> StorageResult<()>) -> StorageResult<()> {
        self.visit_labels_checked(check, |node_id, label_id| {
            check()?;
            if !self.node_exists(node_id)? {
                return Err(StorageError::corrupt(format!(
                    "LabelId {label_id} refers to missing NodeId {node_id}"
                )));
            }
            Ok(())
        })
    }

    fn validate_visible_properties(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        self.visit_properties_observed(check, |owner_kind, owner_id, key_id, _value, _memory| {
            check()?;
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

    fn validate_node_deltas(&self, check: &dyn Fn() -> StorageResult<()>) -> StorageResult<()> {
        for (node_id, op) in &self.state.overlay.nodes {
            check()?;
            require_allocated(self.connection, 1, *node_id, "NodeId")?;
            match op {
                DeltaOp::Add => {}
                DeltaOp::Remove => {
                    if self.node_has_visible_dependents_after(
                        &LayerBuilder::default(),
                        *node_id,
                        check,
                    )? {
                        return Err(StorageError::corrupt(format!(
                            "NodeId {node_id} is removed while visible graph state still references it"
                        )));
                    }
                }
            }
        }
        Ok(())
    }

    fn validate_label_deltas(&self, check: &dyn Fn() -> StorageResult<()>) -> StorageResult<()> {
        for ((node_id, label_id), op) in &self.state.overlay.labels {
            check()?;
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

    fn validate_relationship_deltas(
        &self,
        check: &dyn Fn() -> StorageResult<()>,
    ) -> StorageResult<()> {
        for delta in self.state.overlay.relationships.values() {
            check()?;
            require_allocated(self.connection, 2, delta.record.id, "RelationshipId")?;
            require_allocated(self.connection, 1, delta.record.source, "source NodeId")?;
            require_allocated(self.connection, 1, delta.record.target, "target NodeId")?;
            require_dictionary_id(
                self.connection,
                DictionaryKind::RelationshipType,
                delta.record.type_id,
            )?;
            require_relationship_tuple_stable(self.connection, delta.record)?;
            if delta.op == DeltaOp::Add {
                self.require_visible_relationship_endpoints(delta.record)?;
            }
            if delta.op == DeltaOp::Remove
                && self.owner_has_visible_property_after(
                    &LayerBuilder::default(),
                    OwnerKind::Relationship,
                    delta.record.id,
                    check,
                )?
            {
                return Err(StorageError::corrupt(format!(
                    "RelationshipId {} is removed while visible Properties still reference it",
                    delta.record.id
                )));
            }
        }
        Ok(())
    }

    fn validate_property_deltas(&self, check: &dyn Fn() -> StorageResult<()>) -> StorageResult<()> {
        for ((owner_kind, owner_id, key_id), delta) in &self.state.overlay.properties {
            check()?;
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
        records.retain(|record| !self.state.overlay.relationships.contains_key(&record.id));
        let index = match direction {
            Direction::Outgoing => &self.state.overlay.outgoing,
            Direction::Incoming => &self.state.overlay.incoming,
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
        let Some(checkpoint) = self.state.checkpoint else {
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

pub(super) fn query_local_cache_identity_checked(
    commit: HashId,
    layer: &LayerBuilder,
    schema: Option<&SchemaState>,
    check: &dyn Fn() -> StorageResult<()>,
) -> StorageResult<HashId> {
    let mut hasher = blake3::Hasher::new();
    hasher.update(b"LITHOGRAPH_QUERY_LOCAL_SNAPSHOT_V1");
    hasher.update(commit.as_bytes());
    hasher.update(layer.content_hash_checked(check)?.as_bytes());
    if let Some(schema) = schema {
        let bytes = schema.canonical_blob_checked(check)?;
        for chunk in bytes.chunks(64 * 1024) {
            check()?;
            hasher.update(chunk);
        }
    }
    Ok(HashId::from_bytes(*hasher.finalize().as_bytes()))
}

pub(super) fn advance_query_local_cache_identity(previous: HashId, layer_hash: HashId) -> HashId {
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

pub(super) fn relationship_from_checkpoint_row(
    row: &rusqlite::Row<'_>,
) -> rusqlite::Result<RelationshipRecord> {
    Ok(RelationshipRecord {
        id: row.get(0)?,
        source: row.get(1)?,
        type_id: row.get(2)?,
        target: row.get(3)?,
    })
}
