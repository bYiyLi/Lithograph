//! Incremental Schema impact tracking for an actual mutation Layer.

use std::collections::BTreeSet;

use super::*;
use crate::query::graph::snapshot_observer;
use crate::query::resources::map_entry_bytes;
use crate::storage::{HashId, LayerBuilder};

pub(crate) fn validate_layer_against_commit_schema_controlled(
    connection: &Connection,
    commit: HashId,
    snapshot: &Snapshot<'_>,
    layer: &LayerBuilder,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.check()?;
    if layer.is_empty() {
        return Ok(());
    }
    let observer = snapshot_observer(control.resources());
    let check = || control.check().map_err(QueryError::into_storage_error);
    let (schema, _schema_memory) =
        SchemaState::load_observed(connection, commit, observer.as_ref(), &check)?;
    if schema.graph_nodes.is_empty()
        && schema.graph_relationships.is_empty()
        && schema.constraints.is_empty()
    {
        return Ok(());
    }
    let impact = LayerImpact::resolve(snapshot, layer, control)?;
    let validation = Validation {
        connection,
        snapshot,
        control,
    };
    validation.impacted_nodes(&schema, &impact)?;
    validation.impacted_relationships(&schema, &impact)?;
    validation.impacted_constraints(&schema, &impact)?;
    control.check()
}

impl Validation<'_, '_, '_> {
    fn impacted_nodes(&self, schema: &SchemaState, impact: &LayerImpact) -> QueryResult<()> {
        for definition in schema.graph_nodes.values() {
            self.control.work(1)?;
            let Some(label) = storage::find_label(self.connection, &definition.label)? else {
                continue;
            };
            if impact.node_labels.contains(label) {
                self.graph_node(definition)?;
            }
        }
        Ok(())
    }

    fn impacted_relationships(
        &self,
        schema: &SchemaState,
        impact: &LayerImpact,
    ) -> QueryResult<()> {
        for definition in schema.graph_relationships.values() {
            self.control.work(1)?;
            let Some(type_id) =
                storage::find_relationship_type(self.connection, &definition.relationship_type)?
            else {
                continue;
            };
            let source_changed =
                self.changed_endpoint_label(definition.source_label.as_deref(), impact)?;
            let target_changed =
                self.changed_endpoint_label(definition.target_label.as_deref(), impact)?;
            if impact.relationship_types.contains(type_id) || source_changed || target_changed {
                self.graph_relationship(definition)?;
            }
        }
        Ok(())
    }

    fn changed_endpoint_label(
        &self,
        label: Option<&str>,
        impact: &LayerImpact,
    ) -> QueryResult<bool> {
        let Some(label) = label else {
            return Ok(false);
        };
        self.control.work(1)?;
        Ok(storage::find_label(self.connection, label)?
            .is_some_and(|id| impact.changed_labels.contains(id)))
    }

    fn impacted_constraints(&self, schema: &SchemaState, impact: &LayerImpact) -> QueryResult<()> {
        for constraint in schema.constraints.values() {
            self.control.work(1)?;
            let affected = match &constraint.target {
                SchemaTarget::Node { label } => storage::find_label(self.connection, label)?
                    .is_some_and(|id| impact.node_labels.contains(id)),
                SchemaTarget::Relationship { relationship_type } => {
                    storage::find_relationship_type(self.connection, relationship_type)?
                        .is_some_and(|id| impact.relationship_types.contains(id))
                }
            };
            if affected {
                self.constraint(constraint)?;
            }
        }
        Ok(())
    }
}

struct LayerImpact {
    node_labels: ObservedIds,
    relationship_types: ObservedIds,
    changed_labels: ObservedIds,
}

impl LayerImpact {
    fn resolve(
        snapshot: &Snapshot<'_>,
        layer: &LayerBuilder,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let mut impact = Self {
            node_labels: ObservedIds::new(control)?,
            relationship_types: ObservedIds::new(control)?,
            changed_labels: ObservedIds::new(control)?,
        };
        let mut nodes = ObservedIds::new(control)?;
        let mut relationships = ObservedIds::new(control)?;
        impact.collect_node_changes(layer, &mut nodes, control)?;
        impact.collect_relationship_changes(layer, &mut relationships, control)?;
        collect_property_owners(layer, &mut nodes, &mut relationships, control)?;
        impact.resolve_nodes(snapshot, &nodes, control)?;
        impact.resolve_relationships(snapshot, &relationships, control)?;
        Ok(impact)
    }

    fn collect_node_changes(
        &mut self,
        layer: &LayerBuilder,
        nodes: &mut ObservedIds,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for (node, _) in layer.node_changes() {
            nodes.insert(node, control)?;
        }
        for (node, label, _) in layer.label_changes() {
            nodes.insert(node, control)?;
            self.changed_labels.insert(label, control)?;
        }
        Ok(())
    }

    fn collect_relationship_changes(
        &mut self,
        layer: &LayerBuilder,
        relationships: &mut ObservedIds,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for (relationship, added) in layer.relationship_changes() {
            relationships.insert(relationship.id, control)?;
            if added {
                self.relationship_types
                    .insert(relationship.type_id, control)?;
            }
        }
        Ok(())
    }

    fn resolve_nodes(
        &mut self,
        snapshot: &Snapshot<'_>,
        nodes: &ObservedIds,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let check = || control.check().map_err(QueryError::into_storage_error);
        for &node in &nodes.values {
            control.work(1)?;
            if snapshot.node_exists(node)? {
                snapshot.visit_node_labels_checked(node, &check, |label| {
                    self.node_labels.insert(label, control)
                })?;
            }
        }
        Ok(())
    }

    fn resolve_relationships(
        &mut self,
        snapshot: &Snapshot<'_>,
        relationships: &ObservedIds,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        for &id in &relationships.values {
            control.work(1)?;
            if let Some(record) = snapshot.relationship(id)? {
                self.relationship_types.insert(record.type_id, control)?;
            }
        }
        Ok(())
    }
}

fn collect_property_owners(
    layer: &LayerBuilder,
    nodes: &mut ObservedIds,
    relationships: &mut ObservedIds,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    for (owner, id, _, _) in layer.property_changes() {
        match owner {
            OwnerKind::Node => nodes.insert(id, control)?,
            OwnerKind::Relationship => relationships.insert(id, control)?,
        }
    }
    Ok(())
}

struct ObservedIds {
    values: BTreeSet<i64>,
    memory: MemoryReservation,
}

impl ObservedIds {
    fn new(control: &ExecutionControl<'_>) -> QueryResult<Self> {
        Ok(Self {
            values: BTreeSet::new(),
            memory: control.reserve(0)?,
        })
    }

    fn contains(&self, id: i64) -> bool {
        self.values.contains(&id)
    }

    fn insert(&mut self, id: i64, control: &ExecutionControl<'_>) -> QueryResult<()> {
        control.work(1)?;
        if !self.values.contains(&id) {
            // A singleton owns a whole B-tree node. Charge a conservative leaf
            // and branch allowance before every genuinely new ID insertion.
            self.memory.grow(map_entry_bytes::<i64>()?)?;
            self.values.insert(id);
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::{QueryErrorKind, QueryResources};

    #[test]
    fn removed_endpoint_labels_remain_separate_from_final_node_labels() {
        let connection = super::super::tests::fixture("CREATE (:A:B), (:Deleted)");
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let commit = storage::branch_head(&connection, "main").unwrap();
        let mut snapshot =
            crate::query::graph::resolve_snapshot_with_control(&connection, commit, &control)
                .unwrap();
        let a = storage::find_label(&connection, "A").unwrap().unwrap();
        let b = storage::find_label(&connection, "B").unwrap().unwrap();
        let deleted_label = storage::find_label(&connection, "Deleted")
            .unwrap()
            .unwrap();
        let node = snapshot.scan_label_after(a, 0, 1).unwrap().items[0];
        let deleted = snapshot
            .scan_label_after(deleted_label, 0, 1)
            .unwrap()
            .items[0];
        let mut layer = LayerBuilder::default();
        layer.remove_label(node, a).unwrap();
        layer.remove_node(deleted).unwrap();
        snapshot.apply_layer_checked(&layer, &|| Ok(())).unwrap();
        let retained = resources.used();
        let impact = LayerImpact::resolve(&snapshot, &layer, &control).unwrap();
        assert!(impact.node_labels.contains(b));
        assert!(!impact.node_labels.contains(a));
        assert!(!impact.node_labels.contains(deleted_label));
        assert!(impact.changed_labels.contains(a));
        assert!(impact.relationship_types.values.is_empty());
        drop(impact);
        assert_eq!(resources.used(), retained);
        let restricted = QueryResources::new(Some(2 * map_entry_bytes::<i64>().unwrap()));
        let error = LayerImpact::resolve(
            &snapshot,
            &layer,
            &ExecutionControl::new(&restricted, &|| false),
        )
        .err()
        .unwrap();
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(restricted.used(), 0);
        drop(snapshot);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn impact_uses_property_owners_and_added_but_not_deleted_relationship_types() {
        let connection = super::super::tests::fixture(
            "CREATE (a:C {p:1}), (b:D), (a)-[:R {p:1}]->(b), (a)-[:T]->(b)",
        );
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let commit = storage::branch_head(&connection, "main").unwrap();
        let mut snapshot =
            crate::query::graph::resolve_snapshot_with_control(&connection, commit, &control)
                .unwrap();
        let c = storage::find_label(&connection, "C").unwrap().unwrap();
        let d = storage::find_label(&connection, "D").unwrap().unwrap();
        let r = storage::find_relationship_type(&connection, "R")
            .unwrap()
            .unwrap();
        let t = storage::find_relationship_type(&connection, "T")
            .unwrap()
            .unwrap();
        let s = storage::intern_relationship_type(&connection, "S").unwrap();
        let p = storage::find_property_key(&connection, "p")
            .unwrap()
            .unwrap();
        let live = snapshot
            .scan_relationship_type_after(r, 0, 1)
            .unwrap()
            .items[0];
        let deleted = snapshot
            .scan_relationship_type_after(t, 0, 1)
            .unwrap()
            .items[0];
        let added = RelationshipRecord {
            id: storage::allocate_relationship_id(&connection).unwrap(),
            type_id: s,
            ..live
        };
        let mut layer = LayerBuilder::default();
        layer
            .set_property(OwnerKind::Node, live.source, p, PropertyValue::Integer(2))
            .unwrap();
        layer
            .set_property(
                OwnerKind::Relationship,
                live.id,
                p,
                PropertyValue::Integer(2),
            )
            .unwrap();
        layer.add_relationship(added).unwrap();
        layer.remove_relationship(deleted).unwrap();
        snapshot.apply_layer_checked(&layer, &|| Ok(())).unwrap();
        let retained = resources.used();
        let impact = LayerImpact::resolve(&snapshot, &layer, &control).unwrap();
        assert!(impact.node_labels.contains(c));
        assert!(
            !impact.node_labels.contains(d),
            "relationship endpoints are not implicitly touched"
        );
        assert!(impact.relationship_types.contains(r));
        assert!(impact.relationship_types.contains(s));
        assert!(!impact.relationship_types.contains(t));
        assert!(impact.changed_labels.values.is_empty());
        drop(impact);
        assert_eq!(resources.used(), retained);
        drop(snapshot);
        assert_eq!(resources.used(), 0);
    }
}
