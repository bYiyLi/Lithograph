use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{AstKind, AstNode, ClauseKind, ExpressionKind, QueryConnector, Value};
use crate::storage::{HashId, IndexDefinition, IndexTarget, Snapshot, StandardIndexKind};

use super::super::access::PropertyRules;
use super::super::expression::{
    self, BindingRow, BindingValue, binding_from_value_controlled, compile_expression,
};
use super::super::graph::ResolvedGraphView;
use super::super::options::ExecutionOptions;
use super::super::semantic_index::{
    SemanticEntity, SemanticHit, resolve_semantic_index, vector_initial_candidate_limit,
};
use super::super::spill::distinct_row_key;
use super::super::{QueryError, QueryErrorKind, QueryMetrics, QueryResult};
use super::{
    PreparedProgram, composed_query_parts, executable_query, projection_body, projection_column,
    required_query_body, surface_expressions,
};
use crate::query::resources::{ExecutionControl, MemoryReservation};

pub(crate) enum ConditionalBranchQuery<'a> {
    Body(&'a AstNode),
    Composed(&'a AstNode),
}

pub(crate) fn conditional_branch_query(
    branch: &AstNode,
) -> QueryResult<ConditionalBranchQuery<'_>> {
    if let Some(body) = branch
        .descendants()
        .find(|node| node.kind == AstKind::QueryBody)
    {
        return Ok(ConditionalBranchQuery::Body(body));
    }
    branch
        .children
        .iter()
        .find(|node| node.kind == AstKind::ComposedQuery)
        .map(ConditionalBranchQuery::Composed)
        .ok_or_else(|| QueryError::semantic("conditional branch is missing its query"))
}

pub(crate) fn select_conditional_branch(
    query: &AstNode,
    mut evaluate_when: impl FnMut(&AstNode) -> QueryResult<bool>,
) -> QueryResult<Option<&AstNode>> {
    for branch in &query.children {
        let AstKind::ConditionalBranch(kind) = branch.kind else {
            continue;
        };
        let take = match kind {
            crate::cypher::ConditionalBranchKind::When => {
                let expression = surface_expressions(branch)
                    .into_iter()
                    .next()
                    .ok_or_else(|| QueryError::semantic("WHEN branch is missing its condition"))?;
                evaluate_when(expression)?
            }
            crate::cypher::ConditionalBranchKind::Else => true,
        };
        if take {
            return Ok(Some(branch));
        }
    }
    Ok(None)
}

pub(crate) fn validate_executed_columns(expected: &[String], actual: &[String]) -> QueryResult<()> {
    if expected == actual {
        return Ok(());
    }
    Err(QueryError::internal(format!(
        "prepared columns {expected:?} differ from executed columns {actual:?}"
    )))
}

pub(crate) fn materialize_rows(
    snapshot: &Snapshot<'_>,
    result: RowSet,
    rules: &PropertyRules,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<Vec<Value>>, MemoryReservation)> {
    let mut memory = control.reserve(crate::query::resources::allocation_bytes::<Vec<Value>>(
        result.rows.len(),
    )?)?;
    let mut rows = Vec::with_capacity(result.rows.len());
    for binding in result.rows {
        control.work(1)?;
        memory.grow(crate::query::resources::allocation_bytes::<Value>(
            result.columns.len(),
        )?)?;
        let mut row = Vec::with_capacity(result.columns.len());
        for column in &result.columns {
            let value = expression::binding_value_with_memory(
                snapshot,
                binding.values.get(column).unwrap_or(&BindingValue::Null),
                rules,
                control,
            )?;
            memory.merge(value.reservation)?;
            row.push(value.value);
        }
        rows.push(row);
    }
    Ok((rows, memory))
}

pub(crate) fn restore_global_bindings(
    mut rows: RowSet,
    globals: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<RowSet> {
    rows.track(control)?;
    let mut memory = rows
        .memory
        .take()
        .ok_or_else(|| QueryError::internal("CALL result lost its memory reservation"))?;
    for name in &globals.order {
        control.work(1)?;
        restore_global_column(&mut rows.columns, name, &mut memory, control)?;
        let value = globals.values.get(name).unwrap_or(&BindingValue::Null);
        for row in &mut rows.rows {
            restore_global_value(row, name, value, &mut memory, control)?;
        }
    }
    rows.memory = Some(memory);
    rows.track(control)?;
    Ok(rows)
}

fn restore_global_column(
    columns: &mut Vec<String>,
    name: &str,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    use crate::query::resources::{allocation_bytes, checked_add};

    if !columns.iter().any(|column| column == name) {
        memory.grow(checked_add(
            allocation_bytes::<String>(2)?,
            name.len() as u64,
        )?)?;
        columns.push(expression::clone_string_controlled(name, Some(control))?);
    }
    Ok(())
}

fn restore_global_value(
    row: &mut BindingRow,
    name: &str,
    value: &BindingValue,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};

    control.work(1)?;
    let bytes = checked_add(
        checked_add(
            map_entry_bytes::<BindingValue>()?,
            allocation_bytes::<String>(2)?,
        )?,
        checked_add(
            checked_mul(name.len() as u64, 2)?,
            expression::binding_value_bytes(value, Some(control))?,
        )?,
    )?;
    memory.grow(bytes)?;
    row.insert(
        expression::clone_string_controlled(name, Some(control))?,
        expression::clone_binding_value_unreserved_controlled(value, Some(control))?,
    );
    Ok(())
}

fn match_predicate(clause: &AstNode) -> QueryResult<Option<expression::Expr>> {
    clause
        .children
        .iter()
        .find(|node| node.kind == AstKind::Where)
        .map(|where_node| {
            let predicate = surface_expressions(where_node)
                .into_iter()
                .next()
                .ok_or_else(|| QueryError::semantic("MATCH WHERE is missing its predicate"))?;
            compile_expression(predicate)
        })
        .transpose()
}

fn compile_projection_specs(body: &AstNode, source: &str) -> QueryResult<Vec<ProjectionSpec>> {
    super::direct_projection_items(body)
        .into_iter()
        .map(|item| {
            let expression = surface_expressions(item)
                .into_iter()
                .next()
                .ok_or_else(|| QueryError::semantic("projection item is missing its expression"))?;
            let expression = compile_expression(expression)?;
            Ok(ProjectionSpec {
                column: projection_column(item, source)?,
                aggregate: expr_contains_aggregate(&expression),
                expression,
            })
        })
        .collect()
}

fn projection_output_columns(
    input: &[String],
    specs: &[ProjectionSpec],
    star: bool,
) -> Vec<String> {
    let mut columns = if star {
        let mut columns = input.to_vec();
        columns.sort();
        columns
    } else {
        Vec::new()
    };
    for spec in specs {
        if !columns.contains(&spec.column) {
            columns.push(spec.column.clone());
        }
    }
    columns
}

fn projection_is_distinct(body: &AstNode) -> bool {
    body.children.iter().any(|node| {
        matches!(
            node.kind,
            AstKind::SetQuantifier(crate::cypher::SetQuantifierKind::Distinct)
        )
    })
}

fn grouping_plan(
    specs: &[ProjectionSpec],
    group_by: Option<&AstNode>,
) -> QueryResult<GroupingPlan> {
    let group_by_all = group_by.is_some_and(|group_by| {
        group_by
            .descendants()
            .any(|node| node.kind == AstKind::GroupByAll)
    });
    let explicit_keys = if group_by_all {
        specs
            .iter()
            .filter(|spec| !spec.aggregate)
            .map(|spec| spec.expression.clone())
            .collect()
    } else {
        group_by
            .map(surface_expressions)
            .unwrap_or_default()
            .into_iter()
            .map(compile_expression)
            .collect::<QueryResult<Vec<_>>>()?
    };
    Ok(GroupingPlan {
        explicit_keys,
        group_by_all,
        has_group_by: group_by.is_some(),
    })
}

fn has_grouping_keys(specs: &[ProjectionSpec], plan: &GroupingPlan) -> bool {
    if plan.has_group_by {
        !plan.explicit_keys.is_empty()
    } else {
        specs.iter().any(|spec| !spec.aggregate)
    }
}

pub(crate) fn rows_for_next(previous: &AstNode, mut result: RowSet) -> RowSet {
    if crate::cypher::query_body_returns_columns(previous) {
        return result;
    }
    if crate::cypher::query_body_ends_with_call(previous) {
        result.columns.clear();
        for row in &mut result.rows {
            *row = BindingRow::default();
        }
        return result;
    }
    RowSet::seed()
}

struct ReadExecutor<'connection, 'query> {
    connection: &'connection Connection,
    snapshot: Snapshot<'connection>,
    graph_view: &'query ResolvedGraphView,
    params: &'query BTreeMap<String, Value>,
    source: &'query str,
    metrics: &'query mut QueryMetrics,
    is_interrupted: &'query dyn Fn() -> bool,
    control: &'query ExecutionControl<'query>,
    expression_reservation: MemoryReservation,
    global_bindings: BindingRow,
    options: Option<&'query ExecutionOptions>,
    version_mutated: bool,
    version_summary_commit: Option<HashId>,
}

struct GroupingPlan {
    explicit_keys: Vec<expression::Expr>,
    group_by_all: bool,
    has_group_by: bool,
}

pub(crate) fn execute_version_program(
    connection: &Connection,
    program: &PreparedProgram,
    commit: HashId,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<Vec<Value>>, HashId, MemoryReservation)> {
    if program.writes || !program.version_mutation {
        return Err(QueryError::internal(
            "invalid program reached the Version Procedure executor",
        ));
    }
    let snapshot = super::super::graph::resolve_snapshot_with_control(connection, commit, control)?;
    let (rows, summary_commit, memory) = execute_read_snapshot_with_version_summary(
        connection, program, snapshot, graph_view, params, metrics, control,
    )?;
    Ok((rows, summary_commit.unwrap_or(commit), memory))
}

pub(super) fn execute_read_snapshot_with_version_summary(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<Vec<Value>>, Option<HashId>, MemoryReservation)> {
    let is_interrupted = || control.check().is_err();
    let mut executor = ReadExecutor {
        connection,
        snapshot,
        graph_view,
        params,
        source: &program.source,
        metrics,
        is_interrupted: &is_interrupted,
        control,
        expression_reservation: control.reserve(0)?,
        global_bindings: BindingRow::default(),
        options: Some(&program.options),
        version_mutated: false,
        version_summary_commit: None,
    };
    let result = executor.execute_query_body(&program.root, RowSet::seed())?;
    let (rows, memory) = if program.public_result {
        validate_executed_columns(&program.columns, &result.columns)?;
        materialize_rows(
            &executor.snapshot,
            result,
            &executor.graph_view.query_access,
            control,
        )?
    } else {
        (Vec::new(), control.reserve(0)?)
    };
    Ok((rows, executor.version_summary_commit, memory))
}

#[allow(
    clippy::too_many_arguments,
    reason = "the reusable clause boundary keeps the staged Snapshot and query context explicit"
)]
pub(crate) fn execute_read_clause<'connection>(
    connection: &'connection Connection,
    snapshot: Snapshot<'connection>,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    source: &str,
    clause: &AstNode,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<RowSet> {
    let mut executor = ReadExecutor {
        connection,
        snapshot,
        graph_view,
        params,
        source,
        metrics,
        is_interrupted,
        control,
        expression_reservation: control.reserve(0)?,
        global_bindings: BindingRow::default(),
        options: None,
        version_mutated: false,
        version_summary_commit: None,
    };
    executor.execute_single(
        &AstNode {
            kind: AstKind::SingleQuery,
            span: clause.span,
            text: None,
            children: vec![clause.clone()],
        },
        input,
    )
}

impl ReadExecutor<'_, '_> {
    fn execute_query_body(&mut self, body: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        self.in_expression_scope(|executor| executor.execute_query_body_inner(body, input))
    }

    fn in_expression_scope(
        &mut self,
        run: impl FnOnce(&mut Self) -> QueryResult<RowSet>,
    ) -> QueryResult<RowSet> {
        let enclosing =
            std::mem::replace(&mut self.expression_reservation, self.control.reserve(0)?);
        let mut result = run(self);
        let temporary = std::mem::replace(&mut self.expression_reservation, enclosing);
        if let Ok(rows) = &mut result {
            if let Some(memory) = rows.memory.as_mut() {
                memory.merge(temporary)?;
            } else {
                rows.memory = Some(temporary);
            }
            rows.track(self.control)?;
        }
        result
    }

    fn execute_query_body_inner(&mut self, body: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        self.check_interrupted()?;
        let query = executable_query(body, false)?;
        match query.kind {
            AstKind::ComposedQuery => self.execute_composed(query, input),
            AstKind::ConditionalQuery => self.execute_conditional(query, input),
            _ => Err(QueryError::internal(
                "query-body dispatch selected an invalid AST node",
            )),
        }
    }

    fn execute_composed(&mut self, query: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let parts = composed_query_parts(query)?;
        let mut result = self.execute_operand(parts.first, clone_row_set(&input, self.control)?)?;
        let mut segment_input = clone_row_set(&input, self.control)?;
        let mut previous_operand = parts.first;
        for (connector, operand) in parts.rest {
            self.check_interrupted()?;
            match connector {
                QueryConnector::Next => {
                    segment_input = rows_for_next(previous_operand, result);
                    result = self
                        .execute_operand(operand, clone_row_set(&segment_input, self.control)?)?;
                }
                QueryConnector::Union
                | QueryConnector::UnionAll
                | QueryConnector::UnionDistinct => {
                    let right = self
                        .execute_operand(operand, clone_row_set(&segment_input, self.control)?)?;
                    result = union_rows(
                        result,
                        right,
                        connector != QueryConnector::UnionAll,
                        &self.snapshot,
                        &self.graph_view.query_access,
                        self.control,
                    )?;
                }
            }
            previous_operand = operand;
        }
        Ok(result)
    }

    fn execute_operand(&mut self, operand: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        match operand.kind {
            AstKind::SingleQuery => self.execute_single(operand, input),
            AstKind::Subquery(_) => {
                let body = required_query_body(operand, "braced query is missing its body")?;
                self.execute_query_body(body, input)
            }
            _ => Err(QueryError::internal(
                "composed query contains a non-executable operand",
            )),
        }
    }

    fn execute_conditional(&mut self, query: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let mut output: Option<RowSet> = None;
        for row in input.rows {
            self.check_interrupted()?;
            let Some(branch) = select_conditional_branch(query, |expression| {
                expression::predicate(self.evaluate(&compile_expression(expression)?, &row)?)
            })?
            else {
                continue;
            };
            let branch_result = self.execute_conditional_branch(branch, row)?;
            output = Some(match output {
                Some(current) => union_rows(
                    current,
                    branch_result,
                    false,
                    &self.snapshot,
                    &self.graph_view.query_access,
                    self.control,
                )?,
                None => branch_result,
            });
        }
        Ok(output.unwrap_or(RowSet {
            columns: super::infer_columns(query, &input.columns, self.source)?,
            rows: Vec::new(),
            memory: None,
        }))
    }

    fn execute_conditional_branch(
        &mut self,
        branch: &AstNode,
        row: BindingRow,
    ) -> QueryResult<RowSet> {
        let input = RowSet {
            columns: row.order.clone(),
            rows: vec![row],
            memory: None,
        };
        match conditional_branch_query(branch)? {
            ConditionalBranchQuery::Body(body) => self.execute_query_body(body, input),
            ConditionalBranchQuery::Composed(query) => self.execute_composed(query, input),
        }
    }

    fn execute_single(&mut self, single: &AstNode, rows: RowSet) -> QueryResult<RowSet> {
        self.execute_single_from(single, 0, rows)
    }

    fn execute_single_from(
        &mut self,
        single: &AstNode,
        start: usize,
        mut rows: RowSet,
    ) -> QueryResult<RowSet> {
        for (index, clause) in single.children.iter().enumerate().skip(start) {
            let AstKind::Clause(kind) = clause.kind else {
                continue;
            };
            self.check_interrupted()?;
            if kind == ClauseKind::LoadCsv {
                return load_csv::execute_load_csv_tail(self, single, index, rows);
            }
            rows = self.execute_single_clause(clause, kind, rows)?;
        }
        Ok(rows)
    }

    fn execute_single_clause(
        &mut self,
        clause: &AstNode,
        kind: ClauseKind,
        rows: RowSet,
    ) -> QueryResult<RowSet> {
        let mut output = self.execute_single_clause_unaccounted(clause, kind, rows)?;
        let mut memory =
            std::mem::replace(&mut self.expression_reservation, self.control.reserve(0)?);
        if let Some(output_memory) = output.memory.take() {
            memory.merge(output_memory)?;
        }
        output.memory = Some(memory);
        output.track(self.control)?;
        Ok(output)
    }

    fn execute_single_clause_unaccounted(
        &mut self,
        clause: &AstNode,
        kind: ClauseKind,
        rows: RowSet,
    ) -> QueryResult<RowSet> {
        match kind {
            ClauseKind::Match | ClauseKind::OptionalMatch => {
                self.execute_match(clause, kind == ClauseKind::OptionalMatch, rows)
            }
            ClauseKind::Filter => self.execute_filter(clause, rows),
            ClauseKind::Let => self.execute_let(clause, rows),
            ClauseKind::Unwind | ClauseKind::For => self.execute_unwind(clause, rows),
            ClauseKind::Call => self.execute_call(clause, rows),
            ClauseKind::Show => self.execute_show(clause, rows),
            ClauseKind::With => {
                let projected = self.execute_projection(clause, rows, true)?;
                self.restore_global_bindings(projected)
            }
            ClauseKind::Return => self.execute_projection(clause, rows, false),
            ClauseKind::Finish => Ok(RowSet {
                columns: Vec::new(),
                rows: Vec::new(),
                memory: None,
            }),
            other => Err(QueryError::semantic(format!(
                "Phase 06 read program does not execute {other:?} yet"
            ))),
        }
    }

    fn execute_match(
        &mut self,
        clause: &AstNode,
        optional: bool,
        input: RowSet,
    ) -> QueryResult<RowSet> {
        let columns = self.match_columns(clause, &input.columns)?;
        let predicate = match_predicate(clause)?;
        let access_operator_count = usize::from(
            clause
                .descendants()
                .any(|node| node.kind == AstKind::Search),
        ) + clause
            .descendants()
            .filter(|node| node.kind == AstKind::PatternPart)
            .map(|part| {
                1 + part
                    .descendants()
                    .filter(|node| node.kind == AstKind::RelationshipPattern)
                    .count()
            })
            .sum::<usize>();
        let previous_operator = self
            .metrics
            .activate_access_operator_group(access_operator_count);
        let result = (|| {
            let mut rows = Vec::new();
            let mut memory = self.control.reserve(0)?;
            for input_row in input.rows {
                let baseline = self.expression_reservation.bytes();
                let matched = self.execute_match_row(
                    clause,
                    optional,
                    &columns,
                    predicate.as_ref(),
                    input_row,
                )?;
                let produced = self.expression_reservation.bytes() - baseline;
                let mut matched_memory = self.expression_reservation.split(produced)?;
                let mut bytes = 0;
                for row in &matched {
                    bytes = crate::query::resources::checked_add(
                        bytes,
                        expression::binding_row_bytes_controlled(row, Some(self.control))?,
                    )?;
                }
                matched_memory.resize(bytes)?;
                memory.merge(matched_memory)?;
                rows.extend(matched);
            }
            Ok(RowSet {
                columns,
                rows,
                memory: Some(memory),
            })
        })();
        if let Ok(output) = &result {
            self.metrics
                .record_active_rows(output.rows.len().try_into().unwrap_or(u64::MAX));
        }
        self.metrics.restore_active_operator(previous_operator);
        result
    }

    fn match_columns(&self, clause: &AstNode, input: &[String]) -> QueryResult<Vec<String>> {
        super::infer_columns(
            &AstNode {
                kind: AstKind::SingleQuery,
                span: clause.span,
                text: None,
                children: vec![clause.clone()],
            },
            input,
            self.source,
        )
    }

    fn execute_match_row(
        &mut self,
        clause: &AstNode,
        optional: bool,
        columns: &[String],
        predicate: Option<&expression::Expr>,
        input: BindingRow,
    ) -> QueryResult<Vec<BindingRow>> {
        if let Some(search) = clause
            .descendants()
            .find(|node| node.kind == AstKind::Search)
        {
            return self
                .execute_search_match_row(clause, search, optional, columns, predicate, input);
        }
        let copied =
            RowSet::try_clone_rows(&input.order, std::slice::from_ref(&input), self.control)?;
        let (mut matched, matched_memory) = super::path::execute_match(
            &self.snapshot,
            self.graph_view,
            self.params,
            clause,
            copied.rows,
            self.metrics,
            self.control,
        )?;
        self.expression_reservation.merge(matched_memory)?;
        if let Some(predicate) = predicate {
            matched = self.filter_rows(predicate, matched)?;
        }
        if optional && matched.is_empty() {
            return Ok(vec![null_extend_row(input, columns)]);
        }
        Ok(matched)
    }

    fn execute_search_match_row(
        &mut self,
        clause: &AstNode,
        search: &AstNode,
        optional: bool,
        columns: &[String],
        predicate: Option<&expression::Expr>,
        input: BindingRow,
    ) -> QueryResult<Vec<BindingRow>> {
        search::execute_search_match_row(self, clause, search, optional, columns, predicate, input)
    }

    fn filter_rows(
        &mut self,
        predicate: &expression::Expr,
        rows: Vec<BindingRow>,
    ) -> QueryResult<Vec<BindingRow>> {
        let mut filtered = Vec::new();
        for row in rows {
            if expression::predicate(self.evaluate(predicate, &row)?)? {
                filtered.push(row);
            }
        }
        Ok(filtered)
    }

    fn execute_filter(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let expression = surface_expressions(clause)
            .into_iter()
            .next()
            .ok_or_else(|| QueryError::semantic("FILTER is missing its predicate"))?;
        let expression = compile_expression(expression)?;
        let mut rows = Vec::new();
        for row in input.rows {
            if expression::predicate(self.evaluate(&expression, &row)?)? {
                rows.push(row);
            }
        }
        Ok(RowSet {
            columns: input.columns,
            rows,
            memory: None,
        })
    }

    fn execute_let(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let bindings = expression::compile_let_bindings(clause)?;
        let mut columns = input.columns;
        for (name, _) in &bindings {
            if !columns.contains(name) {
                columns.push(name.clone());
            }
        }
        let mut rows = Vec::with_capacity(input.rows.len());
        for source in input.rows {
            let mut output = source.clone();
            for (name, expression) in &bindings {
                let value = self.evaluate(expression, &source)?;
                output.insert(
                    name.clone(),
                    binding_from_value_controlled(&self.snapshot, value, self.control)?,
                );
            }
            rows.push(output);
        }
        Ok(RowSet {
            columns,
            rows,
            memory: None,
        })
    }

    fn execute_unwind(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let (name, expression) = compile_unwind(clause)?;
        let mut rows = Vec::new();
        let mut memory = self.control.reserve(0)?;
        for input_row in input.rows {
            let value = self.evaluate(&expression, &input_row)?;
            let values = match value {
                Value::Null => Vec::new(),
                Value::List(values) => values,
                value => vec![value],
            };
            for value in values {
                self.control.work(1)?;
                memory.grow(crate::query::resources::checked_add(
                    expression::binding_row_bytes_controlled(&input_row, Some(self.control))?,
                    crate::query::resources::value_bytes(&value)?,
                )?)?;
                let mut row = input_row.clone();
                row.insert(
                    name.clone(),
                    binding_from_value_controlled(&self.snapshot, value, self.control)?,
                );
                rows.push(row);
            }
        }
        let mut columns = input.columns;
        if !columns.contains(&name) {
            columns.push(name);
        }
        Ok(RowSet {
            columns,
            rows,
            memory: Some(memory),
        })
    }

    fn execute_projection(
        &mut self,
        clause: &AstNode,
        input: RowSet,
        apply_where: bool,
    ) -> QueryResult<RowSet> {
        let body = projection_body(clause)?;
        let star = super::has_surface_star(body);
        let specs = compile_projection_specs(body, self.source)?;
        let columns = projection_output_columns(&input.columns, &specs, star);
        let group_by = body
            .children
            .iter()
            .find(|node| node.kind == AstKind::GroupBy);
        let projected = self.project_rows(input, &specs, star, group_by)?;
        let projected = self.apply_projection_modifiers(body, &columns, projected)?;
        let projected = self.apply_projection_where(clause, apply_where, projected)?;
        let rows = projected.into_iter().map(|record| record.output).collect();
        Ok(RowSet {
            columns,
            rows,
            memory: None,
        })
    }

    fn project_rows(
        &mut self,
        input: RowSet,
        specs: &[ProjectionSpec],
        star: bool,
        group_by: Option<&AstNode>,
    ) -> QueryResult<Vec<ProjectedRow>> {
        if group_by.is_some() || specs.iter().any(|spec| spec.aggregate) {
            self.aggregate_projection(input, specs, star, group_by)
        } else {
            self.scalar_projection(input, specs, star)
        }
    }

    fn apply_projection_modifiers(
        &mut self,
        body: &AstNode,
        columns: &[String],
        mut projected: Vec<ProjectedRow>,
    ) -> QueryResult<Vec<ProjectedRow>> {
        if projection_is_distinct(body) {
            projected = distinct_projected(
                projected,
                columns,
                &self.snapshot,
                &self.graph_view.query_access,
                self.control,
            )?;
        }
        if let Some(order) = body
            .children
            .iter()
            .find(|node| node.kind == AstKind::OrderBy)
        {
            self.sort_projected(&mut projected, order)?;
        }
        let skip = pagination(
            body,
            AstKind::Skip,
            "SKIP",
            &self.snapshot,
            self.params,
            &self.graph_view.query_access,
            self.control,
        )?
        .unwrap_or(0);
        let limit = pagination(
            body,
            AstKind::Limit,
            "LIMIT",
            &self.snapshot,
            self.params,
            &self.graph_view.query_access,
            self.control,
        )?;
        Ok(projected
            .into_iter()
            .skip(skip)
            .take(limit.unwrap_or(usize::MAX))
            .collect())
    }

    fn apply_projection_where(
        &mut self,
        clause: &AstNode,
        apply_where: bool,
        projected: Vec<ProjectedRow>,
    ) -> QueryResult<Vec<ProjectedRow>> {
        if !apply_where {
            return Ok(projected);
        }
        let Some(where_node) = clause
            .children
            .iter()
            .find(|node| node.kind == AstKind::Where)
        else {
            return Ok(projected);
        };
        let predicate = surface_expressions(where_node)
            .into_iter()
            .next()
            .ok_or_else(|| QueryError::semantic("WITH WHERE is missing its predicate"))?;
        let predicate = compile_expression(predicate)?;
        let mut filtered = Vec::new();
        for record in projected {
            if expression::predicate(self.evaluate_projected(&predicate, &record)?)? {
                filtered.push(record);
            }
        }
        Ok(filtered)
    }

    fn restore_global_bindings(&self, rows: RowSet) -> QueryResult<RowSet> {
        restore_global_bindings(rows, &self.global_bindings, self.control)
    }

    fn scalar_projection(
        &mut self,
        input: RowSet,
        specs: &[ProjectionSpec],
        star: bool,
    ) -> QueryResult<Vec<ProjectedRow>> {
        self.expression_reservation
            .grow(crate::query::resources::allocation_bytes::<ProjectedRow>(
                input.rows.len(),
            )?)?;
        input
            .rows
            .into_iter()
            .map(|source| {
                self.expression_reservation
                    .grow(crate::query::resources::allocation_bytes::<BindingRow>(1)?)?;
                let mut output = if star {
                    let mut copied = RowSet::try_clone_rows(
                        &source.order,
                        std::slice::from_ref(&source),
                        self.control,
                    )?;
                    if let Some(memory) = copied.memory.take() {
                        self.expression_reservation.merge(memory)?;
                    }
                    copied.rows.pop().ok_or_else(|| {
                        QueryError::internal("star projection lost its copied row")
                    })?
                } else {
                    BindingRow::default()
                };
                preserve_load_csv_context(&source, &mut output);
                for spec in specs {
                    let value = self.evaluate(&spec.expression, &source)?;
                    output.insert(
                        spec.column.clone(),
                        binding_from_value_controlled(&self.snapshot, value, self.control)?,
                    );
                }
                Ok(ProjectedRow {
                    output,
                    group: vec![source],
                })
            })
            .collect()
    }

    fn evaluate(&mut self, expression: &expression::Expr, row: &BindingRow) -> QueryResult<Value> {
        let baseline = self.expression_reservation.bytes();
        let (expression, _memory) =
            self.materialize_subqueries(expression, row, &BTreeMap::new())?;
        let evaluated = expression::evaluate_with_access_control(
            &expression,
            &self.snapshot,
            row,
            self.params,
            &self.graph_view.query_access,
            self.control,
        )?;
        drop(expression);
        drop(_memory);
        self.finish_evaluation(evaluated, baseline)
    }

    fn check_interrupted(&self) -> QueryResult<()> {
        if (self.is_interrupted)() {
            Err(QueryError::interrupted())
        } else {
            Ok(())
        }
    }
}

pub(crate) fn compile_unwind(clause: &AstNode) -> QueryResult<(String, expression::Expr)> {
    let name = clause
        .descendants()
        .find(|node| node.kind == AstKind::BindingVariable)
        .and_then(|node| node.text.clone())
        .ok_or_else(|| QueryError::semantic("UNWIND/FOR is missing its variable"))?;
    let expression = surface_expressions(clause)
        .into_iter()
        .next()
        .ok_or_else(|| QueryError::semantic("UNWIND/FOR is missing its list expression"))?;
    let expression = compile_expression(expression)?;
    Ok((name, expression))
}

fn preserve_load_csv_context(source: &BindingRow, output: &mut BindingRow) {
    output.load_csv_context.clone_from(&source.load_csv_context);
}

fn preserve_common_load_csv_context(group: &[BindingRow], output: &mut BindingRow) {
    let Some(first) = group.first().and_then(|row| row.load_csv_context.as_ref()) else {
        return;
    };
    let file = first.file.as_ref().filter(|value| {
        group.iter().all(|row| {
            row.load_csv_context
                .as_ref()
                .and_then(|context| context.file.as_ref())
                == Some(*value)
        })
    });
    let line = first.line.filter(|value| {
        group.iter().all(|row| {
            row.load_csv_context
                .as_ref()
                .and_then(|context| context.line)
                == Some(*value)
        })
    });
    if file.is_some() || line.is_some() {
        output.load_csv_context = Some(crate::query::expression::LoadCsvContext {
            file: file.cloned(),
            line,
        });
    }
}

mod aggregation;
mod rowset;
mod sorting;
pub(crate) use rowset::RowSet;

mod call;
pub(crate) use call::{append_call_columns, merge_call_result, project_call_bindings};
mod graph_expression;
mod helpers;
mod load_csv;
mod registry;
mod search;

pub(crate) use helpers::{distinct_bindings, merge_union_rows};

use helpers::{
    ProjectedRow, ProjectionSpec, aggregate_average, aggregate_deviation, aggregate_extreme,
    aggregate_percentile, aggregate_sum, clone_row_set, compare_keys, distinct_projected,
    expr_contains_aggregate, null_extend_row, pagination, pattern_expression_clause, union_rows,
};

#[cfg(test)]
mod global_bindings_tests;
