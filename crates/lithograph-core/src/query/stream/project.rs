use super::*;

pub(super) struct ProjectedRow {
    pub(super) values: Vec<Value>,
    pub(super) memory: super::super::MemoryReservation,
}

pub(super) fn project_row(
    prepared: &PreparedQuery,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    control: &super::super::ExecutionControl<'_>,
) -> QueryResult<ProjectedRow> {
    let mut memory = control.reserve(super::super::resources::allocation_bytes::<Value>(
        prepared.projections.len(),
    )?)?;
    let mut values = Vec::with_capacity(prepared.projections.len());
    for projection in &prepared.projections {
        let evaluated = expression::evaluate_with_access_control(
            &projection.expression,
            snapshot,
            row,
            &prepared.params,
            &prepared.graph_view.query_access,
            control,
        )?;
        memory.merge(evaluated.reservation)?;
        values.push(evaluated.value);
    }
    Ok(ProjectedRow { values, memory })
}

pub(super) fn order_values(
    prepared: &PreparedQuery,
    snapshot: &Snapshot<'_>,
    binding: &BindingRow,
    projected: &[Value],
    control: &super::super::ExecutionControl<'_>,
) -> QueryResult<ProjectedRow> {
    let mut aliases_memory = control.reserve(super::super::resources::row_bytes_controlled(
        projected, control,
    )?)?;
    for name in &prepared.columns {
        aliases_memory.grow(super::super::resources::checked_add(
            super::super::resources::map_entry_bytes::<Value>()?,
            name.capacity() as u64,
        )?)?;
    }
    let mut aliases = std::collections::BTreeMap::new();
    for (name, value) in prepared.columns.iter().zip(projected) {
        aliases.insert(
            expression::clone_string_controlled(name, Some(control))?,
            expression::clone_value_unreserved_controlled(value, Some(control))?,
        );
    }
    let mut memory = control.reserve(super::super::resources::allocation_bytes::<Value>(
        prepared.order.len(),
    )?)?;
    let mut values = Vec::with_capacity(prepared.order.len());
    for item in &prepared.order {
        let evaluated = expression::evaluate_with_aliases_and_access_control(
            &item.expression,
            snapshot,
            binding,
            &prepared.params,
            &aliases,
            &prepared.graph_view.query_access,
            control,
        )?;
        memory.merge(evaluated.reservation)?;
        values.push(evaluated.value);
    }
    Ok(ProjectedRow { values, memory })
}
