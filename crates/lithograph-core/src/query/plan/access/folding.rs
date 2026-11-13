//! Prepay candidate cloning only for indexes that can serve this MATCH.

use super::*;

pub(super) fn folding_fits(
    params: &BTreeMap<String, Value>,
    schema: &storage::SchemaState,
    step: &MatchStep,
    inspected_bytes: usize,
    inspected_states: usize,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<bool> {
    if schema.indexes.is_empty() {
        return Ok(true);
    }
    let before_parameters = budget.states;
    let Some(bytes) = budget.parameter_bytes(params, control)? else {
        return Ok(false);
    };
    let parameter_states = budget.states - before_parameters;
    let Some((properties, names)) = applicable_properties(step, schema, budget, control)? else {
        return Ok(false);
    };
    // Keep the base candidate/proof workspace, parameter capacity checks and
    // real catalog inspection. An unrelated index cannot clone a seek here.
    Ok(budget.work(
        properties.saturating_mul(inspected_states.saturating_add(parameter_states).max(1)),
        bytes
            .saturating_add(inspected_bytes)
            .saturating_add(names)
            .saturating_add(128)
            .saturating_mul(properties.saturating_add(1))
            .saturating_mul(4)
            .saturating_mul(step.parts.len()),
    ))
}

fn applicable_properties(
    step: &MatchStep,
    schema: &storage::SchemaState,
    budget: &mut PlanningBudget,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(usize, usize)>> {
    let (mut properties, mut names) = (0usize, 0usize);
    for index in schema.indexes.values() {
        control.work(1)?;
        if !budget.work(1, 0) {
            return Ok(None);
        }
        for part in &step.parts {
            let work = part
                .start
                .label_names
                .len()
                .saturating_mul(2)
                .saturating_add(3);
            control.work(work as u64)?;
            if !budget.work(work, 0) {
                return Ok(None);
            }
            let target_length = match &index.target {
                storage::IndexTarget::NodeProperties { label, .. } => label.len(),
                storage::IndexTarget::RelationshipProperties {
                    relationship_type, ..
                } => relationship_type.len(),
                _ => 0,
            };
            let comparison_bytes = part
                .start
                .label_names
                .iter()
                .fold(0usize, |bytes, name| {
                    bytes.saturating_add(name.len().min(target_length))
                })
                .saturating_add(
                    part.relationship
                        .as_ref()
                        .and_then(|rel| rel.type_name.as_ref())
                        .map_or(0, |name| name.len().min(target_length)),
                );
            if !budget.work(0, comparison_bytes) {
                return Ok(None);
            }
            if let Some(slots) =
                super::super::super::schema::standard_index_part_property_slots(index, part)
            {
                properties = properties.saturating_add(slots);
                names = names.saturating_add(index.name.len());
                break;
            }
        }
    }
    Ok(Some((properties, names)))
}
