use super::*;

impl<'connection, 'query> MutationContext<'connection, 'query> {
    pub(super) fn new(
        connection: &'connection Connection,
        base_commit: HashId,
        params: &'query BTreeMap<String, Value>,
        graph_view_selector: &'query GraphViewSelector,
        access: &'query super::super::super::access::AccessPolicy,
        resources: &'query QueryResources,
        is_interrupted: &'query dyn Fn() -> bool,
    ) -> QueryResult<Self> {
        let base = crate::query::graph::resolve_snapshot_with_control(
            connection,
            base_commit,
            &ExecutionControl::new(resources, is_interrupted),
        )?;
        Ok(Self {
            connection,
            staged: base.clone(),
            base,
            base_commit,
            params,
            graph_view_selector,
            access,
            delta: DeltaBuilder::default(),
            global_bindings: BindingRow::default(),
            control: ExecutionControl::new(resources, is_interrupted),
            mutation_memory: resources.reserve(0)?,
            merge_memory: resources.reserve(0)?,
            property_memory: BTreeMap::new(),
        })
    }

    pub(super) fn staged_snapshot(&self) -> QueryResult<Snapshot<'connection>> {
        Ok(self.staged.clone())
    }

    pub(super) fn validated_layer(
        &self,
    ) -> QueryResult<(LayerBuilder, MutationCounters, Snapshot<'connection>)> {
        self.control.check()?;
        let layer = self.delta.layer(&self.control)?;
        let counters = self.delta.counters(&self.control)?;
        let snapshot = self.staged_snapshot()?;
        crate::query::schema::validate_layer_against_commit_schema_controlled(
            self.connection,
            self.base_commit,
            &snapshot,
            &layer,
            &self.control,
        )?;
        Ok((layer, counters, snapshot))
    }

    pub(super) fn commit_layer(
        &self,
        branch: &str,
        layer: &LayerBuilder,
        metadata: &CommitMetadata,
    ) -> QueryResult<HashId> {
        Ok(storage::commit_layer_observed(
            self.connection,
            branch,
            self.base_commit,
            None,
            layer,
            metadata,
            storage::CommitRead::observed(
                crate::query::graph::snapshot_observer(self.control.resources()),
                &|| storage_checkpoint(&self.control),
            ),
        )?)
    }

    fn apply_staged_layer(&mut self, layer: &LayerBuilder) -> QueryResult<()> {
        let control = &self.control;
        self.staged
            .apply_layer_checked(layer, &|| storage_checkpoint(control))?;
        Ok(())
    }

    pub(super) fn set_node(&mut self, id: i64, current: bool) -> QueryResult<()> {
        if current && !self.access.write.allow_create {
            return Err(QueryError::access_denied("Node creation is not allowed"));
        }
        if !current {
            self.require_delete()?;
        }
        self.control.work(1)?;
        if !self.delta.nodes.contains_key(&id) {
            self.mutation_memory.grow(512)?;
        }
        self.delta.set_node(&self.base, id, current)?;
        let mut layer = LayerBuilder::default();
        if current {
            layer.add_node(id)?;
        } else {
            layer.remove_node(id)?;
        }
        self.apply_staged_layer(&layer)?;
        Ok(())
    }

    pub(super) fn set_label(
        &mut self,
        node_id: i64,
        label_id: i64,
        current: bool,
    ) -> QueryResult<()> {
        self.require_label_changes()?;
        self.set_label_for_delete(node_id, label_id, current)
    }

    pub(super) fn set_label_for_delete(
        &mut self,
        node_id: i64,
        label_id: i64,
        current: bool,
    ) -> QueryResult<()> {
        self.control.work(1)?;
        if !self.delta.labels.contains_key(&(node_id, label_id)) {
            self.mutation_memory.grow(512)?;
        }
        self.delta
            .set_label(&self.base, node_id, label_id, current)?;
        let mut layer = LayerBuilder::default();
        if current {
            layer.add_label(node_id, label_id)?;
        } else {
            layer.remove_label(node_id, label_id)?;
        }
        self.apply_staged_layer(&layer)?;
        Ok(())
    }

    pub(super) fn set_relationship(
        &mut self,
        id: i64,
        current: Option<RelationshipRecord>,
    ) -> QueryResult<()> {
        if current.is_some() && !self.access.write.allow_create {
            return Err(QueryError::access_denied(
                "Relationship creation is not allowed",
            ));
        }
        if current.is_none() {
            self.require_delete()?;
        }
        self.control.work(1)?;
        if !self.delta.relationships.contains_key(&id) {
            self.mutation_memory.grow(768)?;
        }
        let previous = self.staged.relationship(id)?;
        self.delta.set_relationship(&self.base, id, current)?;
        let mut layer = LayerBuilder::default();
        match current {
            Some(record) => layer.add_relationship(record)?,
            None => {
                if let Some(record) = previous {
                    layer.remove_relationship(record)?;
                }
            }
        }
        self.apply_staged_layer(&layer)?;
        Ok(())
    }

    pub(super) fn set_property(
        &mut self,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: i64,
        current: Option<PropertyValue>,
    ) -> QueryResult<()> {
        if !self.access.write.properties.is_empty() {
            let name = storage::property_key_name(self.connection, key_id)?.ok_or_else(|| {
                QueryError::internal("property key is missing from the dictionary")
            })?;
            self.access.check_write_property(&name)?;
        }
        self.set_property_for_delete(owner_kind, owner_id, key_id, current)
    }

    pub(super) fn set_property_for_delete(
        &mut self,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: i64,
        current: Option<PropertyValue>,
    ) -> QueryResult<()> {
        self.control.work(1)?;
        let key = (owner_kind, owner_id, key_id);
        let property_memory = self.reserve_property_change(key, current.as_ref())?;
        let copied = current
            .as_ref()
            .map(|value| {
                storage::clone_property_value_checked(value, &|| storage_checkpoint(&self.control))
            })
            .transpose()?;
        self.delta.set_property(
            &self.base,
            owner_kind,
            owner_id,
            key_id,
            copied,
            &self.control,
        )?;
        let mut layer = LayerBuilder::default();
        match current {
            Some(value) => layer.set_property(owner_kind, owner_id, key_id, value)?,
            None => layer.remove_property(owner_kind, owner_id, key_id)?,
        }
        self.apply_staged_layer(&layer)?;
        self.property_memory.insert(key, property_memory);
        Ok(())
    }

    fn reserve_property_change(
        &self,
        key: (OwnerKind, i64, i64),
        current: Option<&PropertyValue>,
    ) -> QueryResult<MemoryReservation> {
        let check = || storage_checkpoint(&self.control);
        let current_bytes = current
            .map(|value| storage::property_value_memory_bytes(value, &check))
            .transpose()?
            .unwrap_or(0);
        let before_bytes = match self.delta.properties.get(&key) {
            Some(slot) => slot
                .before
                .as_ref()
                .map(|value| storage::property_value_memory_bytes(value, &check))
                .transpose()?
                .unwrap_or(0),
            None => self
                .base
                .with_property_checked(key.0, key.1, key.2, &check, |before| {
                    before
                        .map(|value| storage::property_value_memory_bytes(value, &check))
                        .transpose()
                        .map(|bytes| bytes.unwrap_or(0))
                })?,
        };
        // The new layer, staged snapshot, and delta copies coexist. Keep the
        // existing four-copy workspace, plus the one retained before value.
        self.control.reserve(crate::query::resources::checked_add(
            512,
            crate::query::resources::checked_add(
                before_bytes,
                crate::query::resources::checked_mul(current_bytes, 4)?,
            )?,
        )?)
    }

    pub(super) fn require_delete(&self) -> QueryResult<()> {
        if self.access.write.allow_delete {
            Ok(())
        } else {
            Err(QueryError::access_denied(
                "graph element deletion is not allowed",
            ))
        }
    }

    pub(super) fn require_label_changes(&self) -> QueryResult<()> {
        if self.access.write.allow_label_changes {
            Ok(())
        } else {
            Err(QueryError::access_denied("Label changes are not allowed"))
        }
    }

    pub(super) fn graph_view(&self) -> QueryResult<ResolvedGraphView> {
        Ok(
            ResolvedGraphView::resolve(self.connection, self.graph_view_selector)?
                .with_query_access(self.access.read.query.clone()),
        )
    }

    pub(super) fn validate_view(&self, touched: &TouchedElements) -> QueryResult<()> {
        validate_clause_view(
            &self.staged,
            self.graph_view_selector,
            touched,
            &self.control,
        )
    }
}

fn storage_checkpoint(control: &ExecutionControl<'_>) -> storage::StorageResult<()> {
    control.check().map_err(QueryError::into_storage_error)
}
