use super::*;

impl DeltaBuilder {
    pub(super) fn layer(&self, control: &ExecutionControl<'_>) -> QueryResult<LayerBuilder> {
        let mut layer = LayerBuilder::default();
        self.apply_node_slots(&mut layer, control)?;
        self.apply_label_slots(&mut layer, control)?;
        self.apply_relationship_slots(&mut layer, control)?;
        self.apply_property_slots(&mut layer, control)?;
        Ok(layer)
    }

    pub(super) fn counters(&self, control: &ExecutionControl<'_>) -> QueryResult<MutationCounters> {
        let mut counters = MutationCounters::default();
        self.count_node_slots(&mut counters, control)?;
        self.count_label_slots(&mut counters, control)?;
        self.count_relationship_slots(&mut counters, control)?;
        self.count_property_slots(&mut counters, control)?;
        Ok(counters)
    }

    fn count_node_slots(
        &self,
        counters: &mut MutationCounters,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for slot in self.nodes.values() {
            control.work(1)?;
            if slot.before == slot.current {
                continue;
            }
            if slot.current {
                counters.nodes_created = counters.nodes_created.saturating_add(1);
            } else {
                counters.nodes_deleted = counters.nodes_deleted.saturating_add(1);
            }
        }
        Ok(())
    }

    fn count_label_slots(
        &self,
        counters: &mut MutationCounters,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for slot in self.labels.values() {
            control.work(1)?;
            if slot.before == slot.current {
                continue;
            }
            if slot.current {
                counters.labels_added = counters.labels_added.saturating_add(1);
            } else {
                counters.labels_removed = counters.labels_removed.saturating_add(1);
            }
        }
        Ok(())
    }

    fn count_relationship_slots(
        &self,
        counters: &mut MutationCounters,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for slot in self.relationships.values() {
            control.work(1)?;
            match (slot.before, slot.current) {
                (None, Some(_)) => {
                    counters.relationships_created =
                        counters.relationships_created.saturating_add(1)
                }
                (Some(_), None) => {
                    counters.relationships_deleted =
                        counters.relationships_deleted.saturating_add(1)
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn count_property_slots(
        &self,
        counters: &mut MutationCounters,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for slot in self.properties.values() {
            control.work(1)?;
            if property_states_equal(slot.before.as_ref(), slot.current.as_ref(), control)? {
                continue;
            }
            if slot.before.is_some() {
                counters.properties_removed = counters.properties_removed.saturating_add(1);
            }
            if slot.current.is_some() {
                counters.properties_set = counters.properties_set.saturating_add(1);
            }
        }
        Ok(())
    }

    fn apply_node_slots(
        &self,
        layer: &mut LayerBuilder,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for (&id, slot) in &self.nodes {
            control.work(1)?;
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

    fn apply_label_slots(
        &self,
        layer: &mut LayerBuilder,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for (&(node_id, label_id), slot) in &self.labels {
            control.work(1)?;
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

    fn apply_relationship_slots(
        &self,
        layer: &mut LayerBuilder,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for slot in self.relationships.values() {
            control.work(1)?;
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

    fn apply_property_slots(
        &self,
        layer: &mut LayerBuilder,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for (&(owner_kind, owner_id, key_id), slot) in &self.properties {
            control.work(1)?;
            if property_states_equal(slot.before.as_ref(), slot.current.as_ref(), control)? {
                continue;
            }
            match &slot.current {
                Some(value) => layer.set_property(
                    owner_kind,
                    owner_id,
                    key_id,
                    storage::clone_property_value_checked(value, &|| {
                        control.check().map_err(QueryError::into_storage_error)
                    })?,
                )?,
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
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        use std::collections::btree_map::Entry;
        match self.properties.entry((owner_kind, owner_id, key_id)) {
            Entry::Occupied(mut entry) => entry.get_mut().current = current,
            Entry::Vacant(entry) => {
                let check = || control.check().map_err(QueryError::into_storage_error);
                let before =
                    base.with_property_checked(owner_kind, owner_id, key_id, &check, |value| {
                        value
                            .map(|value| storage::clone_property_value_checked(value, &check))
                            .transpose()
                    })?;
                entry.insert(Slot { before, current });
            }
        }
        Ok(())
    }
}

pub(super) fn property_states_equal(
    left: Option<&PropertyValue>,
    right: Option<&PropertyValue>,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    match (left, right) {
        (None, None) => Ok(true),
        (Some(left), Some(right)) => canonical_values_equal(left, right, control),
        _ => Ok(false),
    }
}

fn canonical_values_equal(
    left: &PropertyValue,
    right: &PropertyValue,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    let check = || control.check().map_err(QueryError::into_storage_error);
    let bytes = crate::query::resources::checked_add(
        storage::property_value_memory_bytes(left, &check)?,
        storage::property_value_memory_bytes(right, &check)?,
    )?;
    let _encoding_memory = control.reserve(bytes)?;
    let left = left.canonical_bytes_checked(&check)?;
    let right = right.canonical_bytes_checked(&check)?;
    if left.len() != right.len() {
        return Ok(false);
    }
    for (left, right) in left.chunks(64 * 1024).zip(right.chunks(64 * 1024)) {
        control.bytes(left.len() as u64)?;
        if left != right {
            return Ok(false);
        }
    }
    Ok(true)
}

#[cfg(test)]
mod tests;
