use super::*;

impl DeltaBuilder {
    pub(super) fn layer(&self) -> QueryResult<LayerBuilder> {
        let mut layer = LayerBuilder::default();
        self.apply_node_slots(&mut layer)?;
        self.apply_label_slots(&mut layer)?;
        self.apply_relationship_slots(&mut layer)?;
        self.apply_property_slots(&mut layer)?;
        Ok(layer)
    }

    pub(super) fn counters(&self) -> QueryResult<MutationCounters> {
        let mut counters = MutationCounters::default();
        for slot in self
            .nodes
            .values()
            .filter(|slot| slot.before != slot.current)
        {
            if slot.current {
                counters.nodes_created = counters.nodes_created.saturating_add(1);
            } else {
                counters.nodes_deleted = counters.nodes_deleted.saturating_add(1);
            }
        }
        for slot in self
            .labels
            .values()
            .filter(|slot| slot.before != slot.current)
        {
            if slot.current {
                counters.labels_added = counters.labels_added.saturating_add(1);
            } else {
                counters.labels_removed = counters.labels_removed.saturating_add(1);
            }
        }
        for slot in self
            .relationships
            .values()
            .filter(|slot| slot.before != slot.current)
        {
            match (slot.before, slot.current) {
                (None, Some(_)) => {
                    counters.relationships_created =
                        counters.relationships_created.saturating_add(1);
                }
                (Some(_), None) => {
                    counters.relationships_deleted =
                        counters.relationships_deleted.saturating_add(1);
                }
                (Some(_), Some(_)) | (None, None) => {}
            }
        }
        for slot in self.properties.values() {
            if property_states_equal(&slot.before, &slot.current)? {
                continue;
            }
            if slot.before.is_some() {
                counters.properties_removed = counters.properties_removed.saturating_add(1);
            }
            if slot.current.is_some() {
                counters.properties_set = counters.properties_set.saturating_add(1);
            }
        }
        Ok(counters)
    }

    fn apply_node_slots(&self, layer: &mut LayerBuilder) -> QueryResult<()> {
        for (&id, slot) in &self.nodes {
            if slot.before == slot.current {
                continue;
            }
            if slot.current {
                layer.add_node(id)?;
            } else {
                layer.remove_node(id)?;
            }
        }
        Ok(())
    }

    fn apply_label_slots(&self, layer: &mut LayerBuilder) -> QueryResult<()> {
        for (&(node_id, label_id), slot) in &self.labels {
            if slot.before == slot.current {
                continue;
            }
            if slot.current {
                layer.add_label(node_id, label_id)?;
            } else {
                layer.remove_label(node_id, label_id)?;
            }
        }
        Ok(())
    }

    fn apply_relationship_slots(&self, layer: &mut LayerBuilder) -> QueryResult<()> {
        for slot in self.relationships.values() {
            if slot.before == slot.current {
                continue;
            }
            match slot.current {
                Some(record) => layer.add_relationship(record)?,
                None => {
                    if let Some(record) = slot.before {
                        layer.remove_relationship(record)?;
                    }
                }
            };
        }
        Ok(())
    }

    fn apply_property_slots(&self, layer: &mut LayerBuilder) -> QueryResult<()> {
        for (&(owner_kind, owner_id, key_id), slot) in &self.properties {
            if property_states_equal(&slot.before, &slot.current)? {
                continue;
            }
            match &slot.current {
                Some(value) => layer.set_property(owner_kind, owner_id, key_id, value.clone())?,
                None => layer.remove_property(owner_kind, owner_id, key_id)?,
            };
        }
        Ok(())
    }

    pub(super) fn set_node(
        &mut self,
        base: &Snapshot<'_>,
        id: i64,
        current: bool,
    ) -> QueryResult<()> {
        let slot = self.nodes.entry(id).or_insert(Slot {
            before: base.node_exists(id)?,
            current: base.node_exists(id)?,
        });
        slot.current = current;
        Ok(())
    }

    pub(super) fn set_label(
        &mut self,
        base: &Snapshot<'_>,
        node_id: i64,
        label_id: i64,
        current: bool,
    ) -> QueryResult<()> {
        let before = base.labels(node_id)?.binary_search(&label_id).is_ok();
        self.labels
            .entry((node_id, label_id))
            .or_insert(Slot {
                before,
                current: before,
            })
            .current = current;
        Ok(())
    }

    pub(super) fn set_relationship(
        &mut self,
        base: &Snapshot<'_>,
        id: i64,
        current: Option<RelationshipRecord>,
    ) -> QueryResult<()> {
        let before = base.relationship(id)?;
        self.relationships
            .entry(id)
            .or_insert(Slot {
                before,
                current: before,
            })
            .current = current;
        Ok(())
    }

    pub(super) fn set_property(
        &mut self,
        base: &Snapshot<'_>,
        owner_kind: OwnerKind,
        owner_id: i64,
        key_id: i64,
        current: Option<PropertyValue>,
    ) -> QueryResult<()> {
        let before = base.property(owner_kind, owner_id, key_id)?;
        self.properties
            .entry((owner_kind, owner_id, key_id))
            .or_insert(Slot {
                before: before.clone(),
                current: before,
            })
            .current = current;
        Ok(())
    }
}

pub(super) fn property_states_equal(
    left: &Option<PropertyValue>,
    right: &Option<PropertyValue>,
) -> QueryResult<bool> {
    match (left, right) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => Ok(left.canonical_bytes()? == right.canonical_bytes()?),
        (None, Some(_)) | (Some(_), None) => Ok(false),
    }
}

pub(super) fn staged_snapshot<'connection>(
    connection: &'connection Connection,
    base_commit: HashId,
    delta: &DeltaBuilder,
) -> QueryResult<Snapshot<'connection>> {
    let layer = delta.layer()?;
    Ok(Snapshot::resolve_with_layer(
        connection,
        base_commit,
        &layer,
    )?)
}
