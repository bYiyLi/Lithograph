//! Keep a fresh plain Node MATCH's pattern barrier without retaining its domain.
//! Candidate enumeration and pattern matching finish before WHERE for each
//! incoming row. Only final survivors enter the existing eager output RowSet.

use super::*;
use crate::query::plan::MatchStep;
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};
use crate::query::spill::{BINDING_SPILL_BATCH_ROWS, BindingSpill, open_spill_connection};
use crate::query::stream::{MatchReadBindings, StepCursor};

pub(super) fn lower_match(
    connection: &Connection,
    clause: &AstNode,
    incoming: &[String],
) -> QueryResult<Option<MatchStep>> {
    if identity::identity_clause_size(clause).is_none()
        || clause.kind != AstKind::Clause(ClauseKind::Match)
    {
        return Ok(None);
    }
    let Some(mut step) = identity::lower_pattern(connection, clause)? else {
        return Ok(None);
    };
    if step.predicate.is_none() {
        return Ok(None);
    }
    let [part] = step.parts.as_slice() else {
        return Ok(None);
    };
    if part.relationship.is_some()
        || part.end.is_some()
        || part.path_variable.is_some()
        || part.start.variable.is_none()
    {
        return Ok(None);
    }
    Ok(crate::query::plan::constrain_match_scope(&mut step, incoming).then_some(step))
}

// The connection owns both TEMP tables and is dropped after each incoming row,
// including any error/interrupt. Nothing accumulates across outer rows and no
// transaction is opened on the host connection.
struct PatternSpools {
    connection: Connection,
    candidates: BindingSpill,
    matched: BindingSpill,
}

impl PatternSpools {
    fn new(control: &ExecutionControl<'_>) -> QueryResult<Self> {
        let connection = open_spill_connection()?;
        connection.execute_batch("BEGIN")?;
        let candidates =
            BindingSpill::create_with_resources(&connection, control.resources().clone())?;
        let matched =
            BindingSpill::create_with_resources(&connection, control.resources().clone())?;
        Ok(Self {
            connection,
            candidates,
            matched,
        })
    }
}

impl ReadExecutor<'_, '_> {
    pub(super) fn execute_single_node_match(
        &mut self,
        clause: &AstNode,
        step: &MatchStep,
        input: RowSet,
    ) -> QueryResult<RowSet> {
        self.execute_node_match_rows(clause, input, |executor, row, rows, memory| {
            executor.append_single_node_fallback(step, row, rows, memory)
        })
    }

    // Both the plain fallback and SIP own the same eager RowSet boundary and
    // restore the previous PROFILE operator even when a row fails or cancels.
    pub(super) fn execute_node_match_rows(
        &mut self,
        clause: &AstNode,
        input: RowSet,
        mut append: impl FnMut(
            &mut Self,
            BindingRow,
            &mut Vec<BindingRow>,
            &mut MemoryReservation,
        ) -> QueryResult<()>,
    ) -> QueryResult<RowSet> {
        let columns = self.match_columns(clause, &input.columns)?;
        let previous = self
            .metrics
            .activate_match_clause_access(clause.span.start, 1);
        let result = (|| {
            let mut memory = self.control.reserve(0)?;
            let mut rows = Vec::new();
            for row in input.rows {
                append(self, row, &mut rows, &mut memory)?;
            }
            Ok(RowSet {
                columns,
                rows,
                memory: Some(memory),
            })
        })();
        self.metrics.restore_active_operator(previous);
        result
    }

    pub(super) fn append_single_node_fallback(
        &mut self,
        step: &MatchStep,
        mut base: BindingRow,
        rows: &mut Vec<BindingRow>,
        memory: &mut MemoryReservation,
    ) -> QueryResult<()> {
        // Relationship uniqueness belongs to this MATCH, as in StepCursor::new.
        base.used_relationships.clear();
        let mut spools = PatternSpools::new(self.control)?;
        self.spill_node_candidates(step, &mut spools)?;
        self.spill_node_pattern(step, &mut spools)?;
        spools.candidates.abort(&spools.connection)?;
        self.filter_node_pattern(step, &base, &spools, rows, memory)?;
        spools.matched.abort(&spools.connection)
    }

    fn spill_node_candidates(
        &mut self,
        step: &MatchStep,
        spools: &mut PatternSpools,
    ) -> QueryResult<()> {
        let spec = &step.parts[0].start;
        if spec.impossible {
            return Ok(());
        }
        let variable = spec.variable.as_deref().ok_or_else(|| {
            QueryError::internal("single-node spill requires its fresh Node variable")
        })?;
        let label = (spec.labels.len() == 1).then(|| spec.labels[0]);
        let mut after = 0;
        loop {
            self.control.check()?;
            let _page = self
                .control
                .reserve(allocation_bytes::<i64>(BINDING_SPILL_BATCH_ROWS)?)?;
            let (page, _) = self.graph_view.scan_nodes_after(
                &self.snapshot,
                label,
                after,
                BINDING_SPILL_BATCH_ROWS,
                self.control,
            )?;
            self.metrics.record_db_hits(page.items.len() as u64);
            for id in page.items {
                let candidate = candidate_row(variable, id, self.control)?;
                spools
                    .candidates
                    .push(&spools.connection, &candidate.0, self.control)?;
            }
            let Some(next) = page.next_after else {
                return Ok(());
            };
            after = next;
        }
    }

    fn spill_node_pattern(
        &mut self,
        step: &MatchStep,
        spools: &mut PatternSpools,
    ) -> QueryResult<()> {
        let mut pattern = step.clone();
        pattern.predicate = None;
        pattern.early_predicates.clear();
        let mut after = -1;
        loop {
            let limit = candidate_batch_limit(&pattern, self.control)?;
            let batch = spools.candidates.read_batch_reserved(
                &spools.connection,
                after,
                limit,
                self.control,
            )?;
            let Some(last) = batch.0.last() else {
                break;
            };
            after = last.0;
            let candidates = candidate_ids(&batch.0, &pattern, self.control)?;
            drop(batch);
            self.spill_node_batch(&pattern, candidates, spools)?;
        }
        self.metrics
            .record_pipeline_rows(spools.matched.len().try_into().unwrap_or(u64::MAX));
        Ok(())
    }

    fn spill_node_batch(
        &mut self,
        pattern: &MatchStep,
        candidates: (Vec<i64>, MemoryReservation),
        spools: &mut PatternSpools,
    ) -> QueryResult<()> {
        // The selected pattern has one fresh Node, static labels and no inline
        // predicates or other bindings. Matching needs no outer payload. Keep
        // that immutable base once, outside this Node-only pattern spool.
        let mut cursor = StepCursor::with_pattern_node_ids(
            pattern,
            BindingRow::default(),
            candidates,
            self.control,
        )?;
        let mut bindings = MatchReadBindings::default();
        while let Some(row) = cursor.next_row(
            &self.snapshot,
            self.graph_view,
            self.params,
            self.metrics,
            &mut bindings,
            self.control,
        )? {
            spools
                .matched
                .push(&spools.connection, &row, self.control)?;
        }
        Ok(())
    }

    fn filter_node_pattern(
        &mut self,
        step: &MatchStep,
        base: &BindingRow,
        spools: &PatternSpools,
        rows: &mut Vec<BindingRow>,
        memory: &mut MemoryReservation,
    ) -> QueryResult<()> {
        let mut after = -1;
        loop {
            // A single decoded binding also bounds variable-width incoming
            // payloads; it does not collect an entire page before filtering.
            let batch =
                spools
                    .matched
                    .read_batch_reserved(&spools.connection, after, 1, self.control)?;
            let Some((sequence, row)) = batch.0.first() else {
                return Ok(());
            };
            after = *sequence;
            let working = merge_node_pattern(base, row, step, self.control)?;
            if self.single_node_predicate(step, &working.0)? {
                self.metrics
                    .record_profile_rows(self.metrics.match_residual_slot(step.clause_start), 1);
                // Both source and combined-row leases remain live until the
                // final RowSet reserves its own claim and accepts ownership.
                identity::push_identity_match_row(rows, memory, working.0, self.control)?;
            }
        }
    }

    fn single_node_predicate(&mut self, step: &MatchStep, row: &BindingRow) -> QueryResult<bool> {
        let Some(predicate) = &step.predicate else {
            return Ok(true);
        };
        let baseline = self.expression_reservation.bytes();
        let value = self.evaluate(predicate, row)?;
        let evaluated = self.take_evaluated_value(value, baseline)?;
        expression::predicate(evaluated.value)
    }
}

fn merge_node_pattern(
    base: &BindingRow,
    matched: &BindingRow,
    step: &MatchStep,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let variable = step.parts[0].start.variable.as_deref().ok_or_else(|| {
        QueryError::internal("single-node spill requires its fresh Node variable")
    })?;
    let Some(BindingValue::Node(id)) = matched.values.get(variable) else {
        return Err(QueryError::internal(
            "single-node spill lost a matched NodeId",
        ));
    };
    let bytes = checked_add(
        expression::binding_row_bytes_controlled(base, Some(control))?,
        checked_add(
            candidate_row_bytes(variable)?,
            allocation_bytes::<String>(base.order.len())?,
        )?,
    )?;
    let memory = control.reserve(bytes)?;
    let mut row = expression::clone_binding_row_unreserved_controlled(base, Some(control))?;
    // Preserve every outer column, its declared order and LOAD CSV context.
    // The per-MATCH used set was already cleared once on this outer base.
    row.insert(variable.to_owned(), BindingValue::Node(*id));
    Ok((row, memory))
}

fn candidate_row(
    variable: &str,
    id: i64,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let memory = control.reserve(candidate_row_bytes(variable)?)?;
    let mut row = BindingRow::default();
    row.insert(variable.to_owned(), BindingValue::Node(id));
    Ok((row, memory))
}

fn candidate_row_bytes(variable: &str) -> QueryResult<u64> {
    checked_add(
        checked_add(
            allocation_bytes::<BindingRow>(1)?,
            map_entry_bytes::<BindingValue>()?,
        )?,
        checked_add(
            allocation_bytes::<String>(4)?,
            (variable.len() as u64).saturating_mul(2),
        )?,
    )
}

fn candidate_batch_limit(
    pattern: &MatchStep,
    control: &ExecutionControl<'_>,
) -> QueryResult<usize> {
    let Some(limit) = control.resources().limit() else {
        return Ok(BINDING_SPILL_BATCH_ROWS);
    };
    let variable = pattern.parts[0].start.variable.as_deref().ok_or_else(|| {
        QueryError::internal("single-node spill requires its fresh Node variable")
    })?;
    let encoded =
        crate::query::spill::identity_binding_encoded_upper([Some(variable), None, None], 0)?;
    // This Node-only record has 16 JSON tokens, independently of name escaping
    // and ID digits. Reuse the reader's DOM allowance and the codec's byte bound.
    let encoded = usize::try_from(encoded).map_err(|_| {
        crate::query::resources::memory_error("candidate spill encoding exceeds addressable memory")
    })?;
    let workspace = crate::query::spill::decoding_capacity_bytes(16, encoded)?;
    let slot = allocation_bytes::<(i64, BindingRow)>(1)?;
    // The reader starts at four slots and grows by doubling. IDs coexist with
    // the decoded page until the full page has been converted and dropped.
    let overhead = checked_add(workspace, checked_mul(slot, 4)?)?;
    let per_row = checked_add(
        candidate_row_bytes(variable)?,
        checked_add(checked_mul(slot, 2)?, allocation_bytes::<i64>(1)?)?,
    )?;
    let available = limit.saturating_sub(control.resources().used());
    let rows = available.saturating_sub(overhead) / per_row;
    Ok(rows.clamp(1, BINDING_SPILL_BATCH_ROWS as u64) as usize)
}

fn candidate_ids(
    batch: &[(i64, BindingRow)],
    pattern: &MatchStep,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<i64>, MemoryReservation)> {
    let memory = control.reserve(allocation_bytes::<i64>(batch.len())?)?;
    let mut ids = Vec::with_capacity(batch.len());
    let variable = pattern.parts[0].start.variable.as_deref().ok_or_else(|| {
        QueryError::internal("single-node spill requires its fresh Node variable")
    })?;
    for (_, row) in batch {
        control.work(1)?;
        let Some(BindingValue::Node(id)) = row.values.get(variable) else {
            return Err(QueryError::internal(
                "single-node spill lost a candidate NodeId",
            ));
        };
        ids.push(*id);
    }
    Ok((ids, memory))
}

#[cfg(test)]
mod tests;
