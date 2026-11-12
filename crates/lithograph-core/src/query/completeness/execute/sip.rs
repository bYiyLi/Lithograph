//! A completed incoming list may advise a fresh, single-node MATCH.
//! The original IN predicate remains the authority; an incomplete or untyped
//! list never becomes an exclusion set. Each cursor uses the current Snapshot.

use super::*;
use crate::query::expression::{BinaryOp, Expr};
use crate::query::plan::MatchStep;
use crate::query::resources::allocation_bytes;

#[derive(Clone, Copy)]
enum ListKind {
    Node,
    ElementId,
}

struct ListSource<'a> {
    variable: &'a str,
    kind: ListKind,
}

pub(super) fn lower_match(
    connection: &Connection,
    clause: &AstNode,
    incoming: &[String],
) -> Option<MatchStep> {
    identity::identity_clause_size(clause)?;
    if clause.kind != AstKind::Clause(ClauseKind::Match) {
        return None;
    }
    let mut step = crate::query::plan::lower_match(connection, clause, false).ok()?;
    let [part] = step.parts.as_slice() else {
        return None;
    };
    if part.relationship.is_some() || part.path_variable.is_some() || part.end.is_some() {
        return None;
    }
    if !crate::query::plan::constrain_match_scope(&mut step, incoming) {
        return None;
    }
    let source = list_source(&step)?;
    incoming
        .iter()
        .any(|name| name == source.variable)
        .then_some(step)
}

fn list_source(step: &MatchStep) -> Option<ListSource<'_>> {
    let Expr::Binary(BinaryOp::In, left, right) = step.predicate.as_ref()? else {
        return None;
    };
    let Expr::Variable(list) = right.as_ref() else {
        return None;
    };
    let target = step.parts.first()?.start.variable.as_deref()?;
    let kind = match left.as_ref() {
        Expr::Variable(variable) if variable == target => ListKind::Node,
        Expr::Function {
            name,
            args,
            distinct: false,
            star: false,
        } if name.eq_ignore_ascii_case("elementId")
            && matches!(args.as_slice(), [Expr::Variable(variable)] if variable == target) =>
        {
            ListKind::ElementId
        }
        _ => return None,
    };
    Some(ListSource {
        variable: list,
        kind,
    })
}

fn canonical_node_id(text: &str) -> Option<i64> {
    let digits = text.strip_prefix("n:")?;
    if digits.is_empty() || digits.len() > 19 || digits.starts_with('0') {
        return None;
    }
    digits
        .bytes()
        .all(|byte| byte.is_ascii_digit())
        .then(|| digits.parse::<i64>().ok())
        .flatten()
        .filter(|id| *id > 0)
}

fn list_id(value: &Value, kind: ListKind) -> Option<Option<i64>> {
    match (value, kind) {
        (Value::Null, _) => Some(None),
        (Value::Node(node), ListKind::Node) => canonical_node_id(&node.element_id).map(Some),
        (Value::String(text), ListKind::ElementId) => canonical_node_id(text).map(Some),
        _ => None,
    }
}

fn node_ids(
    step: &MatchStep,
    row: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(Vec<i64>, MemoryReservation)>> {
    let Some(source) = list_source(step) else {
        return Ok(None);
    };
    let Some(BindingValue::Scalar(Value::List(values))) = row.values.get(source.variable) else {
        return Ok(None);
    };
    let mut count = 0_usize;
    // Validate every element before allocating or excluding any candidate.
    // Mixed-list equality can throw in its original order, so it falls back.
    for value in values {
        control.work(1)?;
        let Some(id) = list_id(value, source.kind) else {
            return Ok(None);
        };
        count += usize::from(id.is_some());
    }
    let Ok(bytes) = allocation_bytes::<i64>(count) else {
        return Ok(None);
    };
    let memory = match control.reserve(bytes) {
        Ok(memory) => memory,
        Err(error) if error.kind == QueryErrorKind::Resource => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut ids = Vec::new();
    if ids.try_reserve_exact(count).is_err() {
        return Ok(None);
    }
    for value in values {
        control.work(1)?;
        if let Some(Some(id)) = list_id(value, source.kind) {
            ids.push(id);
        }
    }
    sort_unique(&mut ids, control)?;
    Ok(Some((ids, memory)))
}

// In-place heapsort polls between comparisons without allocating a second
// vector. Sorting preserves the original scan's ascending NodeId traversal.
fn sort_unique(ids: &mut Vec<i64>, control: &ExecutionControl<'_>) -> QueryResult<()> {
    let len = ids.len();
    for start in (0..len / 2).rev() {
        sift(ids, start, len, control)?;
    }
    for end in (1..ids.len()).rev() {
        ids.swap(0, end);
        sift(ids, 0, end, control)?;
    }
    let mut write = 0;
    for read in 0..ids.len() {
        control.work(1)?;
        if write == 0 || ids[read] != ids[write - 1] {
            ids[write] = ids[read];
            write += 1;
        }
    }
    ids.truncate(write);
    control.check()
}

fn sift(
    ids: &mut [i64],
    mut root: usize,
    end: usize,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    while root < end / 2 {
        control.work(1)?;
        let mut child = 2 * root + 1;
        if child + 1 < end && ids[child] < ids[child + 1] {
            child += 1;
        }
        if ids[root] >= ids[child] {
            break;
        }
        ids.swap(root, child);
        root = child;
    }
    Ok(())
}

impl ReadExecutor<'_, '_> {
    pub(super) fn execute_sip_match(
        &mut self,
        clause: &AstNode,
        step: &MatchStep,
        input: RowSet,
    ) -> QueryResult<RowSet> {
        let columns = self.match_columns(clause, &input.columns)?;
        let previous = self
            .metrics
            .activate_match_clause_access(clause.span.start, 1);
        let result = (|| {
            let mut rows = Vec::new();
            let mut memory = self.control.reserve(0)?;
            for row in input.rows {
                let before = rows.len();
                if let Some((ids, ids_memory)) = node_ids(step, &row, self.control)? {
                    self.append_sip_rows(step, row, (ids, ids_memory), &mut rows, &mut memory)?;
                    self.metrics
                        .record_pipeline_rows((rows.len() - before).try_into().unwrap_or(u64::MAX));
                } else {
                    self.append_sip_fallback(clause, step, &columns, row, &mut rows, &mut memory)?;
                    self.metrics.record_eager_match_rows(
                        clause.span.start,
                        (rows.len() - before).try_into().unwrap_or(u64::MAX),
                    );
                }
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

    fn append_sip_rows(
        &mut self,
        step: &MatchStep,
        row: BindingRow,
        candidates: (Vec<i64>, MemoryReservation),
        rows: &mut Vec<BindingRow>,
        memory: &mut MemoryReservation,
    ) -> QueryResult<()> {
        self.metrics
            .record_semi_mask(candidates.0.len().try_into().unwrap_or(u64::MAX), 0);
        let mut cursor =
            crate::query::stream::StepCursor::with_node_ids(step, row, candidates, self.control)?;
        let mut bindings = crate::query::stream::MatchReadBindings::default();
        while let Some(row) = cursor.next_row(
            &self.snapshot,
            self.graph_view,
            self.params,
            self.metrics,
            &mut bindings,
            self.control,
        )? {
            identity::push_identity_match_row(rows, memory, row, self.control)?;
        }
        Ok(())
    }

    fn append_sip_fallback(
        &mut self,
        clause: &AstNode,
        step: &MatchStep,
        columns: &[String],
        row: BindingRow,
        rows: &mut Vec<BindingRow>,
        memory: &mut MemoryReservation,
    ) -> QueryResult<()> {
        let baseline = self.expression_reservation.bytes();
        let matched =
            self.execute_match_row(clause, false, columns, step.predicate.as_ref(), row)?;
        let produced = self.expression_reservation.bytes() - baseline;
        let mut matched_memory = self.expression_reservation.split(produced)?;
        matched_memory.resize(matched_binding_bytes(&matched, self.control)?)?;
        memory.merge(matched_memory)?;
        rows.extend(matched);
        Ok(())
    }
}
