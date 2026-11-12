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
            self.aggregate_state = Some(self.aggregate_counts(snapshot, &control)?);
            // One global aggregate row exists before downstream pagination.
            let slot = self.metrics.operator_profile().and_then(|operators| {
                operators
                    .iter()
                    .position(|operator| operator.operator == "Aggregate")
            });
            self.metrics.record_profile_rows(slot, 1);
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

    fn aggregate_counts(
        &mut self,
        snapshot: &Snapshot<'_>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Vec<u64>> {
        let mut counts = vec![0_u64; self.prepared.projections.len()];
        let mut flatten: Option<super::factor::FlattenContinuation> = None;
        loop {
            control.check()?;
            if self.count_flattened(&mut counts, snapshot, &mut flatten, control)? {
                continue;
            }
            let Some(packet) = self.pipeline.next_packet(
                snapshot,
                &self.prepared.graph_view,
                &self.prepared.params,
                &mut self.metrics,
                control,
            )?
            else {
                break;
            };
            self.count_packet(&mut counts, snapshot, &mut flatten, packet, control)?;
        }
        Ok(counts)
    }

    fn count_flattened(
        &mut self,
        counts: &mut [u64],
        snapshot: &Snapshot<'_>,
        flatten: &mut Option<super::factor::FlattenContinuation>,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        let Some(flatten) = flatten.as_mut() else {
            return Ok(false);
        };
        let clause = flatten.clause_start();
        let row = flatten.with_next_row(control, |row, projections| {
            if let Some(clause) = clause {
                self.metrics.record_factorized_flatten(clause, 1);
            }
            match projections {
                Some(projections) => {
                    let (binding, _memory) = super::factor::consume::project_binding(
                        projections,
                        row,
                        snapshot,
                        &self.prepared.params,
                        &self.prepared.graph_view.query_access,
                        control,
                    )?;
                    self.count_binding(counts, snapshot, &binding, control)
                }
                None => self.count_binding(counts, snapshot, row, control),
            }
        })?;
        if let Some((clause, delta)) = flatten.take_storage_metrics()? {
            self.metrics
                .record_factorized_storage(clause, "Flatten", delta);
        }
        Ok(row.is_some())
    }

    fn count_packet(
        &mut self,
        counts: &mut [u64],
        snapshot: &Snapshot<'_>,
        flatten: &mut Option<super::factor::FlattenContinuation>,
        packet: super::factor::BindingPacket,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        match packet {
            super::factor::BindingPacket::Flat(row) => {
                self.count_binding(counts, snapshot, &row, control)?
            }
            super::factor::BindingPacket::Group(group) => {
                if let Some((values, _memory)) = super::program::group_counts(
                    &group,
                    &self.prepared.projections,
                    snapshot,
                    &self.prepared.params,
                    &self.prepared.graph_view,
                    control,
                    &mut self.metrics,
                )? {
                    if values.len() != counts.len() {
                        return Err(QueryError::internal(
                            "factor COUNT columns differ from projection",
                        ));
                    }
                    for (count, value) in counts.iter_mut().zip(values) {
                        *count = count.saturating_add(value);
                    }
                } else {
                    if flatten.is_none() {
                        *flatten = Some(super::factor::FlattenContinuation::new(control)?);
                    }
                    flatten
                        .as_mut()
                        .ok_or_else(|| QueryError::internal("missing count factor continuation"))?
                        .install(super::factor::BindingPacket::Group(group), control)?;
                }
            }
        }
        Ok(())
    }

    fn count_binding(
        &mut self,
        counts: &mut [u64],
        snapshot: &Snapshot<'_>,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        self.metrics.record_pipeline_rows(1);
        for (count, projection) in counts.iter_mut().zip(&self.prepared.projections) {
            if expression::count_contributes_with_access_control(
                &projection.expression,
                snapshot,
                row,
                &self.prepared.params,
                &self.prepared.graph_view.query_access,
                Some(control),
            )? {
                *count = count.saturating_add(1);
            }
        }
        Ok(())
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
