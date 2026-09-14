use super::*;

impl QueryCursor {
    pub(super) fn next_aggregate(
        &mut self,
        snapshot: &Snapshot<'_>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        if self.aggregate_state.is_none() {
            let mut counts = vec![0_u64; self.prepared.projections.len()];
            loop {
                check_interrupted(is_interrupted)?;
                let Some(row) = self.pipeline.next_row(
                    snapshot,
                    &self.prepared.graph_view,
                    &self.prepared.params,
                    &mut self.metrics,
                )?
                else {
                    break;
                };
                self.metrics.record_active_rows(1);
                for (index, projection) in self.prepared.projections.iter().enumerate() {
                    if expression::count_contributes(
                        &projection.expression,
                        snapshot,
                        &row,
                        &self.prepared.params,
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
            .collect();
        self.metrics.rows = 1;
        self.finish(vec![row])
    }
}

pub(super) fn project_row(
    prepared: &PreparedQuery,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
) -> QueryResult<Vec<Value>> {
    prepared
        .projections
        .iter()
        .map(|projection| {
            expression::evaluate(&projection.expression, snapshot, row, &prepared.params)
        })
        .collect()
}

pub(super) fn order_values(
    prepared: &PreparedQuery,
    snapshot: &Snapshot<'_>,
    binding: &BindingRow,
    projected: &[Value],
) -> QueryResult<Vec<Value>> {
    let aliases = prepared
        .columns
        .iter()
        .cloned()
        .zip(projected.iter().cloned())
        .collect::<std::collections::BTreeMap<_, _>>();
    prepared
        .order
        .iter()
        .map(|item| {
            expression::evaluate_with_aliases(
                &item.expression,
                snapshot,
                binding,
                &prepared.params,
                &aliases,
            )
        })
        .collect()
}
