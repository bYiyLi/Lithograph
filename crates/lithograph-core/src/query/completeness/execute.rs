use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{AstKind, AstNode, ClauseKind, ExpressionKind, QueryConnector, Value};
use crate::storage::{HashId, IndexDefinition, IndexTarget, Snapshot, StandardIndexKind};

use super::super::expression::{
    self, BindingRow, BindingValue, binding_from_value, binding_value, compile_expression,
};
use super::super::graph::ResolvedGraphView;
use super::super::options::ExecutionOptions;
use super::super::semantic_index::{
    SemanticEntity, SemanticHit, resolve_semantic_index, vector_candidates,
    vector_initial_candidate_limit,
};
use super::super::spill::distinct_row_key;
use super::super::{QueryError, QueryErrorKind, QueryMetrics, QueryResult};
use super::{
    PreparedProgram, composed_query_parts, executable_query, projection_body, projection_column,
    required_query_body, surface_expressions,
};

pub(crate) struct RowSet {
    pub(crate) columns: Vec<String>,
    pub(crate) rows: Vec<BindingRow>,
}

impl RowSet {
    pub(crate) fn seed() -> Self {
        Self {
            columns: Vec::new(),
            rows: vec![BindingRow::default()],
        }
    }
}

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

pub(crate) fn restore_global_bindings(mut rows: RowSet, globals: &BindingRow) -> RowSet {
    for name in &globals.order {
        if !rows.columns.contains(name) {
            rows.columns.push(name.clone());
        }
        let value = globals
            .values
            .get(name)
            .cloned()
            .unwrap_or(BindingValue::Null);
        for row in &mut rows.rows {
            row.insert(name.clone(), value.clone());
        }
    }
    rows
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
    body.descendants().any(|node| {
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

pub(super) fn execute_read_with_version_summary(
    connection: &Connection,
    program: &PreparedProgram,
    commit: HashId,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<(Vec<Vec<Value>>, Option<HashId>)> {
    execute_read_snapshot_with_version_summary(
        connection,
        program,
        Snapshot::resolve(connection, commit)?,
        graph_view,
        params,
        metrics,
        is_interrupted,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the version-procedure boundary keeps immutable query execution inputs explicit"
)]
pub(crate) fn execute_version_program(
    connection: &Connection,
    program: &PreparedProgram,
    commit: HashId,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<(Vec<Vec<Value>>, HashId)> {
    if program.writes || !program.version_mutation {
        return Err(QueryError::internal(
            "invalid program reached the Version Procedure executor",
        ));
    }
    let (rows, summary_commit) = execute_read_with_version_summary(
        connection,
        program,
        commit,
        graph_view,
        params,
        metrics,
        is_interrupted,
    )?;
    Ok((rows, summary_commit.unwrap_or(commit)))
}

pub(super) fn execute_read_snapshot(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<Vec<Value>>> {
    execute_read_snapshot_with_version_summary(
        connection,
        program,
        snapshot,
        graph_view,
        params,
        metrics,
        is_interrupted,
    )
    .map(|(rows, _)| rows)
}

fn execute_read_snapshot_with_version_summary(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<(Vec<Vec<Value>>, Option<HashId>)> {
    let mut executor = ReadExecutor {
        connection,
        snapshot,
        graph_view,
        params,
        source: &program.source,
        metrics,
        is_interrupted,
        global_bindings: BindingRow::default(),
        options: Some(&program.options),
        version_mutated: false,
        version_summary_commit: None,
    };
    let result = executor.execute_query_body(&program.root, RowSet::seed())?;
    let rows = if program.public_result {
        validate_executed_columns(&program.columns, &result.columns)?;
        result
            .rows
            .into_iter()
            .map(|row| {
                result
                    .columns
                    .iter()
                    .map(|column| {
                        binding_value(
                            &executor.snapshot,
                            row.values.get(column).unwrap_or(&BindingValue::Null),
                        )
                    })
                    .collect::<QueryResult<Vec<_>>>()
            })
            .collect::<QueryResult<Vec<_>>>()?
    } else {
        Vec::new()
    };
    Ok((rows, executor.version_summary_commit))
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
) -> QueryResult<RowSet> {
    let mut executor = ReadExecutor {
        connection,
        snapshot,
        graph_view,
        params,
        source,
        metrics,
        is_interrupted,
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
        let mut result = self.execute_operand(parts.first, clone_row_set(&input))?;
        let mut segment_input = clone_row_set(&input);
        let mut previous_operand = parts.first;
        for (connector, operand) in parts.rest {
            self.check_interrupted()?;
            match connector {
                QueryConnector::Next => {
                    segment_input = rows_for_next(previous_operand, result);
                    result = self.execute_operand(operand, clone_row_set(&segment_input))?;
                }
                QueryConnector::Union
                | QueryConnector::UnionAll
                | QueryConnector::UnionDistinct => {
                    let right = self.execute_operand(operand, clone_row_set(&segment_input))?;
                    result = union_rows(
                        result,
                        right,
                        connector != QueryConnector::UnionAll,
                        &self.snapshot,
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
                Some(current) => union_rows(current, branch_result, false, &self.snapshot)?,
                None => branch_result,
            });
        }
        Ok(output.unwrap_or(RowSet {
            columns: super::infer_columns(query, &input.columns, self.source)?,
            rows: Vec::new(),
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
                Ok(self.restore_global_bindings(projected))
            }
            ClauseKind::Return => self.execute_projection(clause, rows, false),
            ClauseKind::Finish => Ok(RowSet {
                columns: Vec::new(),
                rows: Vec::new(),
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
            for input_row in input.rows {
                rows.extend(self.execute_match_row(
                    clause,
                    optional,
                    &columns,
                    predicate.as_ref(),
                    input_row,
                )?);
            }
            Ok(RowSet { columns, rows })
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
        let mut matched = super::path::execute_match(
            &self.snapshot,
            self.graph_view,
            self.params,
            clause,
            vec![input.clone()],
            self.metrics,
            self.is_interrupted,
        )?;
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
        })
    }

    fn execute_let(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let mut binding_nodes = clause
            .descendants()
            .filter(|node| node.kind == AstKind::LetBinding)
            .collect::<Vec<_>>();
        binding_nodes.sort_by_key(|node| node.span.start);
        let bindings = binding_nodes
            .into_iter()
            .map(|binding| {
                let name = binding
                    .descendants()
                    .find(|node| node.kind == AstKind::BindingVariable)
                    .and_then(|node| node.text.clone())
                    .ok_or_else(|| QueryError::semantic("LET binding is missing its variable"))?;
                let expression = surface_expressions(binding)
                    .into_iter()
                    .next()
                    .ok_or_else(|| QueryError::semantic("LET binding is missing its expression"))?;
                Ok((name, compile_expression(expression)?))
            })
            .collect::<QueryResult<Vec<_>>>()?;
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
                output.insert(name.clone(), binding_from_value(&self.snapshot, value)?);
            }
            rows.push(output);
        }
        Ok(RowSet { columns, rows })
    }

    fn execute_unwind(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
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
        let mut rows = Vec::new();
        for input_row in input.rows {
            let value = self.evaluate(&expression, &input_row)?;
            let values = match value {
                Value::Null => Vec::new(),
                Value::List(values) => values,
                value => vec![value],
            };
            for value in values {
                let mut row = input_row.clone();
                row.insert(name.clone(), binding_from_value(&self.snapshot, value)?);
                rows.push(row);
            }
        }
        let mut columns = input.columns;
        if !columns.contains(&name) {
            columns.push(name);
        }
        Ok(RowSet { columns, rows })
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
        Ok(RowSet { columns, rows })
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
            projected = distinct_projected(projected, columns, &self.snapshot)?;
        }
        if let Some(order) = body
            .descendants()
            .find(|node| node.kind == AstKind::OrderBy)
        {
            self.sort_projected(&mut projected, order)?;
        }
        let skip =
            pagination(body, AstKind::Skip, "SKIP", &self.snapshot, self.params)?.unwrap_or(0);
        let limit = pagination(body, AstKind::Limit, "LIMIT", &self.snapshot, self.params)?;
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

    fn restore_global_bindings(&self, rows: RowSet) -> RowSet {
        restore_global_bindings(rows, &self.global_bindings)
    }

    fn scalar_projection(
        &mut self,
        input: RowSet,
        specs: &[ProjectionSpec],
        star: bool,
    ) -> QueryResult<Vec<ProjectedRow>> {
        input
            .rows
            .into_iter()
            .map(|source| {
                let mut output = if star {
                    source.clone()
                } else {
                    BindingRow::default()
                };
                preserve_load_csv_context(&source, &mut output);
                for spec in specs {
                    let value = self.evaluate(&spec.expression, &source)?;
                    output.insert(
                        spec.column.clone(),
                        binding_from_value(&self.snapshot, value)?,
                    );
                }
                Ok(ProjectedRow {
                    output,
                    group: vec![source],
                })
            })
            .collect()
    }

    fn aggregate_projection(
        &mut self,
        input: RowSet,
        specs: &[ProjectionSpec],
        star: bool,
        group_by: Option<&AstNode>,
    ) -> QueryResult<Vec<ProjectedRow>> {
        if star {
            return Err(QueryError::semantic(
                "RETURN/WITH * cannot be combined with aggregation",
            ));
        }
        let plan = grouping_plan(specs, group_by)?;
        let mut groups = self.build_aggregate_groups(input.rows, specs, &plan)?;
        if groups.is_empty() && !has_grouping_keys(specs, &plan) {
            groups.insert("[]".to_owned(), Vec::new());
        }
        self.project_aggregate_groups(groups, specs)
    }

    fn build_aggregate_groups(
        &mut self,
        rows: Vec<BindingRow>,
        specs: &[ProjectionSpec],
        plan: &GroupingPlan,
    ) -> QueryResult<BTreeMap<String, Vec<BindingRow>>> {
        let mut groups: BTreeMap<String, Vec<BindingRow>> = BTreeMap::new();
        for row in rows {
            let aliases = self.projection_alias_values(specs, &row)?;
            let values = self.grouping_values(specs, plan, &row, &aliases)?;
            groups
                .entry(distinct_row_key(&values)?)
                .or_default()
                .push(row);
        }
        Ok(groups)
    }

    fn grouping_values(
        &mut self,
        specs: &[ProjectionSpec],
        plan: &GroupingPlan,
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Vec<Value>> {
        if !plan.has_group_by {
            return specs
                .iter()
                .filter(|spec| !spec.aggregate)
                .map(|spec| self.evaluate(&spec.expression, row))
                .collect();
        }
        plan.explicit_keys
            .iter()
            .map(|key| {
                if plan.group_by_all {
                    self.evaluate(key, row)
                } else {
                    self.evaluate_with_aliases(key, row, aliases)
                }
            })
            .collect()
    }

    fn project_aggregate_groups(
        &mut self,
        groups: BTreeMap<String, Vec<BindingRow>>,
        specs: &[ProjectionSpec],
    ) -> QueryResult<Vec<ProjectedRow>> {
        let mut projected = Vec::with_capacity(groups.len());
        for group in groups.into_values() {
            let representative = group.first().cloned().unwrap_or_default();
            let mut output = BindingRow::default();
            preserve_common_load_csv_context(&group, &mut output);
            for spec in specs {
                let value = self.evaluate_group_expression(
                    &spec.expression,
                    &group,
                    &representative,
                    &BTreeMap::new(),
                )?;
                output.insert(
                    spec.column.clone(),
                    binding_from_value(&self.snapshot, value)?,
                );
            }
            projected.push(ProjectedRow { output, group });
        }
        Ok(projected)
    }

    fn projection_alias_values(
        &mut self,
        specs: &[ProjectionSpec],
        row: &BindingRow,
    ) -> QueryResult<BTreeMap<String, Value>> {
        let mut aliases = BTreeMap::new();
        for spec in specs.iter().filter(|spec| !spec.aggregate) {
            aliases.insert(spec.column.clone(), self.evaluate(&spec.expression, row)?);
        }
        Ok(aliases)
    }

    fn evaluate_with_aliases(
        &mut self,
        expression: &expression::Expr,
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Value> {
        let expression = self.materialize_subqueries(expression, row)?;
        expression::evaluate_with_aliases(&expression, &self.snapshot, row, self.params, aliases)
    }

    fn evaluate_group_expression(
        &mut self,
        expression: &expression::Expr,
        group: &[BindingRow],
        representative: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Value> {
        let expression = self.materialize_group_expression(expression, group, representative)?;
        expression::evaluate_with_aliases(
            &expression,
            &self.snapshot,
            representative,
            self.params,
            aliases,
        )
    }

    fn materialize_group_expression(
        &mut self,
        value: &expression::Expr,
        group: &[BindingRow],
        representative: &BindingRow,
    ) -> QueryResult<expression::Expr> {
        expression::transform_expression(value, &mut |candidate| {
            use expression::Expr;
            let replacement = match candidate {
                Expr::Function {
                    name,
                    args,
                    distinct,
                    star,
                } if super::super::registry::is_aggregating(name) => {
                    Some(self.evaluate_aggregate(name, args, *distinct, *star, group)?)
                }
                _ => self.evaluate_graph_runtime_candidate(candidate, representative)?,
            };
            Ok(replacement.map(Expr::Literal))
        })
    }

    fn evaluate_aggregate(
        &mut self,
        name: &str,
        args: &[expression::Expr],
        distinct: bool,
        star: bool,
        group: &[BindingRow],
    ) -> QueryResult<Value> {
        let lower = name.to_ascii_lowercase();
        if lower == "count" && (star || args.is_empty()) {
            return Ok(Value::Integer(group.len() as i64));
        }
        let Some(argument) = args.first() else {
            return Err(QueryError::semantic(format!(
                "{name}() requires an argument"
            )));
        };
        let mut values = Vec::with_capacity(group.len());
        for row in group {
            let value = self.evaluate(argument, row)?;
            if !matches!(value, Value::Null) {
                values.push(value);
            }
        }
        if distinct {
            let mut seen = BTreeSet::new();
            let mut unique = Vec::with_capacity(values.len());
            for value in values {
                if seen.insert(distinct_row_key(std::slice::from_ref(&value))?) {
                    unique.push(value);
                }
            }
            values = unique;
        }
        match lower.as_str() {
            "count" => Ok(Value::Integer(values.len() as i64)),
            "collect" | "collect_list" => Ok(Value::List(values)),
            "sum" => aggregate_sum(&values),
            "avg" => aggregate_average(&values),
            "min" => aggregate_extreme(&values, false),
            "max" => aggregate_extreme(&values, true),
            "percentilecont" | "percentile_cont" => {
                let percentile = self.aggregate_percentile_argument(name, args, group)?;
                aggregate_percentile(&values, percentile, true)
            }
            "percentiledisc" | "percentile_disc" => {
                let percentile = self.aggregate_percentile_argument(name, args, group)?;
                aggregate_percentile(&values, percentile, false)
            }
            "stdev" | "stdev_samp" => aggregate_deviation(&values, true),
            "stdevp" | "stdev_pop" => aggregate_deviation(&values, false),
            _ => Err(QueryError::internal(format!(
                "registered aggregate {name} has no evaluator"
            ))),
        }
    }

    fn aggregate_percentile_argument(
        &mut self,
        name: &str,
        args: &[expression::Expr],
        group: &[BindingRow],
    ) -> QueryResult<Value> {
        if args.len() != 2 {
            return Err(QueryError::semantic(format!(
                "{name}() expects value and percentile arguments"
            )));
        }
        let empty = BindingRow::default();
        self.evaluate(&args[1], group.first().unwrap_or(&empty))
    }

    fn evaluate_projected(
        &mut self,
        expression: &expression::Expr,
        record: &ProjectedRow,
    ) -> QueryResult<Value> {
        let aliases = record
            .output
            .values
            .iter()
            .map(|(name, value)| Ok((name.clone(), binding_value(&self.snapshot, value)?)))
            .collect::<QueryResult<BTreeMap<_, _>>>()?;
        let representative = record.group.first().unwrap_or(&record.output);
        self.evaluate_group_expression(expression, &record.group, representative, &aliases)
    }

    fn sort_projected(&mut self, rows: &mut [ProjectedRow], order: &AstNode) -> QueryResult<()> {
        let expressions = surface_expressions(order)
            .into_iter()
            .map(compile_expression)
            .collect::<QueryResult<Vec<_>>>()?;
        let directions = order
            .descendants()
            .filter_map(|node| match node.kind {
                AstKind::OrderDirection(direction) => Some((node.span.start, direction)),
                _ => None,
            })
            .collect::<Vec<_>>();
        let mut keyed = Vec::with_capacity(rows.len());
        for row in rows.iter().cloned() {
            let mut keys = Vec::with_capacity(expressions.len());
            for expression in &expressions {
                keys.push(self.evaluate_projected(expression, &row)?);
            }
            keyed.push((row, keys));
        }
        for (_, keys) in &keyed {
            compare_keys(keys, keys, &directions, order)?;
        }
        let mut failure = None;
        keyed.sort_by(|left, right| {
            if failure.is_some() {
                return Ordering::Equal;
            }
            match compare_keys(&left.1, &right.1, &directions, order) {
                Ok(ordering) => ordering,
                Err(error) => {
                    failure = Some(error);
                    Ordering::Equal
                }
            }
        });
        if let Some(error) = failure {
            return Err(error);
        }
        for (target, (row, _)) in rows.iter_mut().zip(keyed) {
            *target = row;
        }
        Ok(())
    }

    fn evaluate(&mut self, expression: &expression::Expr, row: &BindingRow) -> QueryResult<Value> {
        let expression = self.materialize_subqueries(expression, row)?;
        expression::evaluate(&expression, &self.snapshot, row, self.params)
    }

    fn check_interrupted(&self) -> QueryResult<()> {
        if (self.is_interrupted)() {
            Err(QueryError::interrupted())
        } else {
            Ok(())
        }
    }
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

mod call;
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
