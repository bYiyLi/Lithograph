use super::*;

impl<'connection, 'query> MutationContext<'connection, 'query> {
    pub(super) fn new(
        connection: &'connection Connection,
        base_commit: HashId,
        params: &'query BTreeMap<String, Value>,
        graph_view_selector: &'query GraphViewSelector,
        access: &'query super::super::super::access::AccessPolicy,
    ) -> QueryResult<Self> {
        let base = Snapshot::resolve(connection, base_commit)?;
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
        })
    }

    pub(super) fn staged_snapshot(&self) -> QueryResult<Snapshot<'connection>> {
        Ok(self.staged.clone())
    }

    pub(super) fn set_node(&mut self, id: i64, current: bool) -> QueryResult<()> {
        if current && !self.access.write.allow_create {
            return Err(QueryError::access_denied("Node creation is not allowed"));
        }
        if !current {
            self.require_delete()?;
        }
        self.delta.set_node(&self.base, id, current)?;
        let mut layer = LayerBuilder::default();
        if current {
            layer.add_node(id)?;
        } else {
            layer.remove_node(id)?;
        }
        self.staged.apply_layer(&layer)?;
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
        self.delta
            .set_label(&self.base, node_id, label_id, current)?;
        let mut layer = LayerBuilder::default();
        if current {
            layer.add_label(node_id, label_id)?;
        } else {
            layer.remove_label(node_id, label_id)?;
        }
        self.staged.apply_layer(&layer)?;
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
        self.staged.apply_layer(&layer)?;
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
        self.delta
            .set_property(&self.base, owner_kind, owner_id, key_id, current.clone())?;
        let mut layer = LayerBuilder::default();
        match current {
            Some(value) => layer.set_property(owner_kind, owner_id, key_id, value)?,
            None => layer.remove_property(owner_kind, owner_id, key_id)?,
        }
        self.staged.apply_layer(&layer)?;
        Ok(())
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
            self.connection,
            self.base_commit,
            &self.delta,
            self.graph_view_selector,
            touched,
        )
    }
}
