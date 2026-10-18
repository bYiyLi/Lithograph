use super::*;

impl QueryCursor {
    pub(super) fn next_aggregate(
        &mut self,
        snapshot: &Snapshot<'_>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        let resources = self.prepared.resources.clone();
        let control = super::super::ExecutionControl::new(&resources, is_interrupted);
        let _counts_memory = control.reserve(super::super::resources::allocation_bytes::<u64>(
            self.prepared.projections.len(),
        )?)?;
        if self.aggregate_state.is_none() {
            let mut counts = vec![0_u64; self.prepared.projections.len()];
            loop {
                check_interrupted(is_interrupted)?;
                let Some(row) = self.pipeline.next_row(
                    snapshot,
                    &self.prepared.graph_view,
                    &self.prepared.params,
                    &mut self.metrics,
                    &control,
                )?
                else {
                    break;
                };
                self.metrics.record_active_rows(1);
                for (index, projection) in self.prepared.projections.iter().enumerate() {
                    if expression::count_contributes_with_access_control(
                        &projection.expression,
                        snapshot,
                        &row,
                        &self.prepared.params,
                        &self.prepared.graph_view.query_access,
                        Some(&control),
                    )? {
                        counts[index] = counts[index].saturating_add(1);
                    }
                }
            }
            self.aggregate_state = Some(counts);
        }
        if self.prepared.skip > 0 || self.prepared.limit == Some(0) {
            return self.finish(Vec::new());
        }
        let counts = self.aggregate_state.take().unwrap_or_default();
        let row = counts
            .into_iter()
            .map(|value| Value::Integer(i64::try_from(value).unwrap_or(i64::MAX)))
            .collect::<Vec<_>>();
        self.batch_memory = Some(control.reserve(super::super::resources::row_bytes(&row)?)?);
        self.metrics.rows = 1;
        self.finish(vec![row])
    }
}

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
