//! Identity access at an eager clause boundary uses the current staged graph.
//! Only structure and a stable ID are selected; no base Schema/type proof or
//! Graph View membership survives a preceding write or transaction boundary.

use super::*;
use crate::query::plan::MatchStep;
use crate::query::resources::allocation_bytes;

pub(crate) fn identity_clause_size(clause: &AstNode) -> Option<(usize, usize)> {
    let bytes = clause.span.end.checked_sub(clause.span.start)?;
    let nodes = clause.descendants().take(513).count();
    (nodes <= 512 && bytes <= 64 * 1024)
        .then(|| (nodes, bytes.saturating_mul(4).saturating_add(nodes * 256)))
}

pub(crate) fn lower_identity_match(
    connection: &Connection,
    clause: &AstNode,
    params: &BTreeMap<String, Value>,
    incoming: &[String],
) -> QueryResult<Option<MatchStep>> {
    if identity_clause_size(clause).is_none()
        || super::super::has_graph_expression(clause, false)
        || clause.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::Subquery(_)
                    | AstKind::Expression(
                        crate::cypher::ExpressionKind::Subquery
                            | crate::cypher::ExpressionKind::InterpolatedString
                    )
            )
        })
    {
        return Ok(None);
    }
    let Some(mut step) = lower_pattern(connection, clause)? else {
        return Ok(None);
    };
    // A single fixed-hop part keeps this boundary on the established scalar
    // cursor. General graph expressions and multi-part programs retain their
    // existing matcher, including its evaluation and relationship-bag order.
    if step.parts.len() != 1 {
        return Ok(None);
    }
    if !crate::query::plan::constrain_match_scope(&mut step, incoming) {
        return Ok(None);
    }
    crate::query::plan::select_node_id_seeks(&mut step, params);
    crate::query::plan::select_relationship_id_seeks(&mut step, params);
    Ok((step.parts[0].start.id_seek.is_some()
        || step.parts[0]
            .relationship
            .as_ref()
            .is_some_and(|rel| rel.id_seek.is_some()))
    .then_some(step))
}

pub(crate) fn lower_pattern(
    connection: &Connection,
    clause: &AstNode,
) -> QueryResult<Option<MatchStep>> {
    match crate::query::plan::lower_match(
        connection,
        clause,
        clause.kind == AstKind::Clause(ClauseKind::OptionalMatch),
    ) {
        Ok(step) => Ok(Some(step)),
        // The fixed-pattern lowerer rejects shapes the legacy matcher supports.
        // A dictionary read failure is not an optimization refusal or retry.
        Err(error) if error.kind == QueryErrorKind::Semantic && error.sqlite_code.is_none() => {
            Ok(None)
        }
        Err(error) => Err(error),
    }
}

impl ReadExecutor<'_, '_> {
    pub(super) fn permitted_identity_match(
        &self,
        clause: &AstNode,
        incoming: &[String],
    ) -> QueryResult<Option<MatchStep>> {
        if !self.identity_match_spans.contains(&clause.span.start) {
            return Ok(None);
        }
        lower_identity_match(self.connection, clause, self.params, incoming)
    }

    pub(super) fn execute_identity_match(
        &mut self,
        clause: &AstNode,
        step: &MatchStep,
        input: RowSet,
    ) -> QueryResult<RowSet> {
        let columns = self.match_columns(clause, &input.columns)?;
        let part = &step.parts[0];
        let accesses = 1 + usize::from(
            part.relationship
                .as_ref()
                .is_some_and(|rel| rel.id_seek.is_none()),
        );
        let previous = self
            .metrics
            .activate_match_clause_access(clause.span.start, accesses);
        let result = (|| {
            let mut rows = Vec::new();
            let mut memory = self.control.reserve(0)?;
            let mut bindings = crate::query::stream::MatchReadBindings::default();
            for row in input.rows {
                let mut cursor = crate::query::stream::StepCursor::new(step, row, self.control)?;
                loop {
                    self.control.work(1)?;
                    let Some(row) = cursor.next_row(
                        &self.snapshot,
                        self.graph_view,
                        self.params,
                        self.metrics,
                        &mut bindings,
                        self.control,
                    )?
                    else {
                        break;
                    };
                    push_identity_match_row(&mut rows, &mut memory, row, self.control)?;
                }
            }
            Ok(RowSet {
                columns,
                rows,
                memory: Some(memory),
            })
        })();
        if let Ok(output) = &result {
            self.metrics
                .record_pipeline_rows(output.rows.len().try_into().unwrap_or(u64::MAX));
        }
        self.metrics.restore_active_operator(previous);
        result
    }
}

pub(super) fn push_identity_match_row(
    rows: &mut Vec<BindingRow>,
    memory: &mut MemoryReservation,
    row: BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    if rows.len() == rows.capacity() {
        let additional = rows.capacity().max(1);
        memory.grow(allocation_bytes::<BindingRow>(additional)?)?;
        rows.try_reserve_exact(additional).map_err(|_| {
            crate::query::resources::memory_error("unable to allocate identity match rows")
        })?;
    }
    memory.grow(
        expression::binding_row_bytes_controlled(&row, Some(control))?
            - allocation_bytes::<BindingRow>(1)?,
    )?;
    rows.push(row);
    Ok(())
}
