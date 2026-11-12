//! Facts from the same immutable Schema as the already admitted Index template.
use super::*;
use crate::storage::{
    ConstraintDefinitionKind, IndexDefinition, IndexTarget, PropertyType, SchemaTarget,
};

#[derive(Clone, Copy, Debug, Default)]
pub(super) struct KeyFacts {
    pub(super) unique: bool,
    pub(super) exists: bool,
    pub(super) integer: bool,
}

pub(super) fn unique(index: &IndexDefinition, schema: &SchemaState) -> bool {
    let Some(constraint) = index
        .owning_constraint
        .as_ref()
        .and_then(|name| schema.constraints.get(name))
    else {
        return false;
    };
    matches!(
        constraint.kind,
        ConstraintDefinitionKind::Key | ConstraintDefinitionKind::Unique
    ) && same_target(index, constraint)
}

pub(super) fn collect(
    index: &IndexDefinition,
    context: &JoinPlanningContext<'_, '_, '_>,
    budget: &mut PlanningBudget,
    headroom: usize,
) -> QueryResult<Option<KeyFacts>> {
    let work = context.schema.constraints.len().saturating_add(1);
    if work.saturating_add(headroom)
        > crate::query::plan::budget::MAX_STATES.saturating_sub(budget.states)
        || !budget.work(work, 0)
    {
        return Ok(None);
    }
    let mut facts = KeyFacts::default();
    for constraint in context.schema.constraints.values() {
        context.control.work(1)?;
        let units = comparison_bytes(index, constraint).div_ceil(64);
        if units.saturating_add(headroom)
            > crate::query::plan::budget::MAX_STATES.saturating_sub(budget.states)
            || !budget.work(units, 0)
        {
            return Ok(None);
        }
        context.control.work(units as u64)?;
        // Reject composite definitions before comparing any property bytes.
        if constraint.properties.len() != 1 || !same_target(index, constraint) {
            continue;
        }
        match &constraint.kind {
            ConstraintDefinitionKind::Key => {
                facts.unique = true;
                facts.exists = true;
            }
            ConstraintDefinitionKind::Unique => facts.unique = true,
            ConstraintDefinitionKind::NotNull => facts.exists = true,
            ConstraintDefinitionKind::Type { rule } => {
                facts.integer |= rule.property_type == PropertyType::Integer;
                facts.exists |= rule.required;
            }
        }
    }
    Ok(Some(facts))
}

fn comparison_bytes(
    index: &IndexDefinition,
    constraint: &crate::storage::ConstraintDefinition,
) -> usize {
    let target = match &index.target {
        IndexTarget::NodeProperties { label, .. } => label.len(),
        IndexTarget::RelationshipProperties {
            relationship_type, ..
        } => relationship_type.len(),
        _ => 0,
    };
    let other = match &constraint.target {
        SchemaTarget::Node { label } => label.len(),
        SchemaTarget::Relationship { relationship_type } => relationship_type.len(),
    };
    target
        .saturating_add(other)
        .saturating_add(constraint.properties.first().map_or(0, String::len))
}

fn same_target(index: &IndexDefinition, constraint: &crate::storage::ConstraintDefinition) -> bool {
    let (scope, properties) = match (&index.target, &constraint.target) {
        (
            IndexTarget::NodeProperties { label, properties },
            SchemaTarget::Node { label: scope },
        ) => (label == scope, properties),
        (
            IndexTarget::RelationshipProperties {
                relationship_type,
                properties,
            },
            SchemaTarget::Relationship {
                relationship_type: scope,
            },
        ) => (relationship_type == scope, properties),
        _ => return false,
    };
    scope && properties.len() == 1 && properties == &constraint.properties
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::{ConstraintDefinition, StandardIndexKind};

    fn index() -> IndexDefinition {
        IndexDefinition {
            name: "key".into(),
            kind: StandardIndexKind::Range,
            target: IndexTarget::NodeProperties {
                label: "Probe".into(),
                properties: vec!["k".into()],
            },
            owning_constraint: Some("key".into()),
            labels_or_types: Vec::new(),
            additional_properties: Vec::new(),
            configuration: None,
        }
    }

    fn schema(kind: ConstraintDefinitionKind) -> SchemaState {
        let mut schema = SchemaState::default();
        schema.constraints.insert(
            "key".into(),
            ConstraintDefinition {
                name: "key".into(),
                target: SchemaTarget::Node {
                    label: "Probe".into(),
                },
                properties: vec!["k".into()],
                kind,
                origin: None,
            },
        );
        schema
    }

    #[test]
    fn same_pin_single_column_key_and_unique_share_the_index_loop_bound() {
        for kind in [
            ConstraintDefinitionKind::Key,
            ConstraintDefinitionKind::Unique,
        ] {
            assert!(unique(&index(), &schema(kind)));
        }
        assert!(!unique(
            &index(),
            &schema(ConstraintDefinitionKind::NotNull)
        ));
        assert!(!unique(&index(), &SchemaState::default()));
    }

    #[test]
    fn other_scope_composite_and_replaced_definition_do_not_supply_uniqueness() {
        let mut schema = schema(ConstraintDefinitionKind::Key);
        let mut other = index();
        if let IndexTarget::NodeProperties { label, .. } = &mut other.target {
            *label = "Other".into();
        }
        assert!(!unique(&other, &schema));
        schema
            .constraints
            .get_mut("key")
            .unwrap()
            .properties
            .push("second".into());
        assert!(!unique(&index(), &schema));
        let mut missing = index();
        missing.owning_constraint = None;
        assert!(!unique(&missing, &schema));
    }
}
