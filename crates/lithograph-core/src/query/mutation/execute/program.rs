use std::collections::BTreeMap;

use crate::cypher::{AstKind, AstNode, ClauseKind, QueryConnector, SubqueryKind, Value};
use crate::query::completeness::execute::{
    ConditionalBranchQuery, RowSet, conditional_branch_query, distinct_bindings,
    execute_read_clause, merge_union_rows, restore_global_bindings, rows_for_next,
    select_conditional_branch as select_branch, validate_executed_columns,
};
use crate::query::completeness::{
    PreparedProgram, composed_query_parts, executable_query, explicit_imports, project_bindings,
    required_query_body,
};
use crate::query::expression::{self, BindingRow, BindingValue, compile_expression};
use crate::query::spill::BindingSpill;

use super::value::materialize_binding_rows_to_spill;
use super::*;

mod load_csv;

use load_csv::execute_load_csv_tail;

pub(crate) struct TransactionBatchOutcome {
    pub(crate) rows: RowSet,
    pub(crate) commit: HashId,
    pub(crate) counters: MutationCounters,
}

pub(crate) struct TransactionMutationContext<'a> {
    pub(crate) connection: &'a Connection,
    pub(crate) program: &'a PreparedProgram,
    pub(crate) base_commit: HashId,
    pub(crate) branch: &'a str,
    pub(crate) graph_view: &'a crate::query::options::GraphViewSelector,
    pub(crate) author: Option<&'a str>,
    pub(crate) message: Option<&'a str>,
    pub(crate) params: &'a BTreeMap<String, Value>,
    pub(crate) metrics: &'a mut QueryMetrics,
    pub(crate) is_interrupted: &'a dyn Fn() -> bool,
}

pub(crate) fn execute_program(
    connection: &Connection,
    program: &PreparedProgram,
    base_commit: HashId,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<WriteOutcome> {
    let options = program
        .write_options
        .as_ref()
        .ok_or_else(|| QueryError::internal("mutating program is missing write options"))?;
    let mut context = MutationContext::new(connection, base_commit, params, &options.graph_view)?;
    let result = execute_query_body(
        &mut context,
        program,
        &program.root,
        RowSet::seed(),
        metrics,
        is_interrupted,
    )?;
    finish_program(context, program, result, is_interrupted)
}

pub(crate) fn execute_transaction_batch(
    context: &mut TransactionMutationContext<'_>,
    subquery: &AstNode,
    input: RowSet,
) -> QueryResult<TransactionBatchOutcome> {
    execute_owned_sqlite_transaction(
        context,
        "CALL subqueries IN TRANSACTIONS require an autocommit Native execution",
        "transaction batch",
        |context| execute_transaction_batch_inner(context, subquery, input),
    )
}

pub(crate) fn execute_program_suffix_transaction(
    context: &mut TransactionMutationContext<'_>,
    single: &AstNode,
    start: usize,
    input: RowSet,
) -> QueryResult<TransactionBatchOutcome> {
    execute_owned_sqlite_transaction(
        context,
        "outer mutation after IN TRANSACTIONS requires Native autocommit execution",
        "outer suffix",
        |context| execute_program_suffix_inner(context, single, start, input),
    )
}

pub(crate) fn execute_spilled_outer_write_transaction(
    context: &mut TransactionMutationContext<'_>,
    clauses: &[AstNode],
    spill_connection: Connection,
    input: BindingSpill,
) -> QueryResult<TransactionBatchOutcome> {
    execute_owned_sqlite_transaction(
        context,
        "outer mutation after IN TRANSACTIONS requires SQLite autocommit execution",
        "outer suffix",
        move |context| execute_spilled_outer_write_inner(context, clauses, spill_connection, input),
    )
}

fn execute_owned_sqlite_transaction(
    context: &mut TransactionMutationContext<'_>,
    boundary_message: &str,
    failure_label: &str,
    operation: impl FnOnce(&mut TransactionMutationContext<'_>) -> QueryResult<TransactionBatchOutcome>,
) -> QueryResult<TransactionBatchOutcome> {
    if !context.connection.is_autocommit() {
        return Err(QueryError::transaction_boundary_required(boundary_message));
    }
    context.connection.execute_batch("BEGIN IMMEDIATE")?;
    let result = operation(context);
    match result {
        Ok(outcome) => {
            if let Err(error) = context.connection.execute_batch("COMMIT") {
                let _ = context.connection.execute_batch("ROLLBACK");
                return Err(error.into());
            }
            Ok(outcome)
        }
        Err(error) => {
            let rollback = context.connection.execute_batch("ROLLBACK");
            if let Err(rollback_error) = rollback {
                return Err(QueryError::internal(format!(
                    "{failure_label} failed ({error}); rollback also failed ({rollback_error})"
                )));
            }
            Err(error)
        }
    }
}

fn execute_program_suffix_inner(
    transaction: &mut TransactionMutationContext<'_>,
    single: &AstNode,
    start: usize,
    input: RowSet,
) -> QueryResult<TransactionBatchOutcome> {
    check_interrupted(transaction.is_interrupted)?;
    let mut context = MutationContext::new(
        transaction.connection,
        transaction.base_commit,
        transaction.params,
        transaction.graph_view,
    )?;
    let rows = execute_single_from(
        &mut context,
        transaction.program,
        single,
        start,
        input,
        transaction.metrics,
        transaction.is_interrupted,
    )?;
    finish_transaction_mutation(transaction, context, rows, true)
}

fn execute_spilled_outer_write_inner(
    transaction: &mut TransactionMutationContext<'_>,
    clauses: &[AstNode],
    spill_connection: Connection,
    mut rows: BindingSpill,
) -> QueryResult<TransactionBatchOutcome> {
    check_interrupted(transaction.is_interrupted)?;
    let mut context = MutationContext::new(
        transaction.connection,
        transaction.base_commit,
        transaction.params,
        transaction.graph_view,
    )?;
    for clause in clauses {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        check_interrupted(transaction.is_interrupted)?;
        let write_clause = match kind {
            ClauseKind::Create | ClauseKind::Insert => {
                Some(WriteClause::Create(lower_write_pattern(clause)?))
            }
            ClauseKind::Set => Some(WriteClause::Set(lower_set_items(clause)?)),
            ClauseKind::Remove => Some(WriteClause::Remove(lower_remove_items(clause)?)),
            ClauseKind::Merge => Some(WriteClause::Merge(lower_merge(clause)?)),
            ClauseKind::Finish => None,
            _ => {
                return Err(QueryError::internal(
                    "unsupported clause reached spilled outer-write executor",
                ));
            }
        };
        if let Some(write_clause) = write_clause {
            rows = execute_clause_spilled(
                &mut context,
                &write_clause,
                &spill_connection,
                rows,
                transaction.metrics,
                transaction.is_interrupted,
            )?;
        }
    }
    rows.abort(&spill_connection)?;
    finish_transaction_mutation(
        transaction,
        context,
        RowSet {
            columns: Vec::new(),
            rows: Vec::new(),
        },
        true,
    )
}

fn execute_transaction_batch_inner(
    transaction: &mut TransactionMutationContext<'_>,
    subquery: &AstNode,
    input: RowSet,
) -> QueryResult<TransactionBatchOutcome> {
    check_interrupted(transaction.is_interrupted)?;
    let body = required_query_body(subquery, "CALL subquery is missing its query body")?;
    let mutated = crate::query::completeness::contains_mutation(body);
    let mut context = MutationContext::new(
        transaction.connection,
        transaction.base_commit,
        transaction.params,
        transaction.graph_view,
    )?;
    let rows = execute_call_subquery_rows(
        &mut context,
        transaction.program,
        subquery,
        body,
        input,
        transaction.metrics,
        transaction.is_interrupted,
    )?;
    finish_transaction_mutation(transaction, context, rows, mutated)
}

fn finish_transaction_mutation(
    transaction: &TransactionMutationContext<'_>,
    context: MutationContext<'_, '_>,
    rows: RowSet,
    commit_mutation: bool,
) -> QueryResult<TransactionBatchOutcome> {
    let final_layer = context.delta.layer()?;
    let counters = context.delta.counters()?;
    let final_snapshot = Snapshot::resolve_with_layer(
        transaction.connection,
        transaction.base_commit,
        &final_layer,
    )?;
    crate::query::schema::validate_layer_against_commit_schema(
        transaction.connection,
        transaction.base_commit,
        &final_snapshot,
        &final_layer,
    )?;
    check_interrupted(transaction.is_interrupted)?;
    let commit = if commit_mutation {
        let metadata = CommitMetadata {
            author: transaction.author.map(str::to_owned),
            message: transaction.message.map(str::to_owned),
            committed_at: now_micros()?,
        };
        storage::commit_layer(
            transaction.connection,
            transaction.branch,
            transaction.base_commit,
            None,
            &final_layer,
            &metadata,
        )?
    } else {
        transaction.base_commit
    };
    Ok(TransactionBatchOutcome {
        rows,
        commit,
        counters,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "CALL transaction batches reuse ordinary CALL correlation semantics"
)]
fn execute_call_subquery_rows(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    subquery: &AstNode,
    body: &AstNode,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let scope = subquery
        .children
        .iter()
        .find(|node| node.kind == AstKind::SubqueryScope);
    let imports = explicit_imports(scope, &input.columns);
    let returns_rows = crate::cypher::query_body_returns_columns(body);
    let mut columns = input.columns.clone();
    let mut output = Vec::new();
    for outer in input.rows {
        check_interrupted(is_interrupted)?;
        let imported = project_bindings(&outer, &imports);
        let globals = if scope.is_some() {
            imported.clone()
        } else {
            BindingRow::default()
        };
        let previous_globals = std::mem::replace(&mut context.global_bindings, globals);
        let inner = if scope.is_some() {
            execute_query_body(
                context,
                program,
                body,
                RowSet {
                    columns: imported.order.clone(),
                    rows: vec![imported],
                },
                metrics,
                is_interrupted,
            )
        } else {
            execute_call_body_with_importing_with(
                context,
                program,
                body,
                &outer,
                metrics,
                is_interrupted,
            )
        };
        context.global_bindings = previous_globals;
        let inner = inner?;
        merge_call_result(outer, inner, returns_rows, &mut columns, &mut output);
    }
    Ok(RowSet {
        columns,
        rows: output,
    })
}

fn execute_query_body(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    body: &AstNode,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let query = executable_query(body, false)?;
    match query.kind {
        AstKind::ComposedQuery => {
            execute_composed(context, program, query, input, metrics, is_interrupted)
        }
        AstKind::ConditionalQuery => {
            execute_conditional(context, program, query, input, metrics, is_interrupted)
        }
        _ => Err(QueryError::internal("invalid program query-body dispatch")),
    }
}

fn execute_composed(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    query: &AstNode,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let parts = composed_query_parts(query)?;
    let mut result = execute_operand(
        context,
        program,
        parts.first,
        clone_rows(&input),
        metrics,
        is_interrupted,
    )?;
    let mut segment_input = clone_rows(&input);
    let mut previous_operand = parts.first;
    for (connector, operand) in parts.rest {
        check_interrupted(is_interrupted)?;
        match connector {
            QueryConnector::Next => {
                segment_input = rows_for_next(previous_operand, result);
                result = execute_operand(
                    context,
                    program,
                    operand,
                    clone_rows(&segment_input),
                    metrics,
                    is_interrupted,
                )?;
            }
            QueryConnector::Union | QueryConnector::UnionAll | QueryConnector::UnionDistinct => {
                let right = execute_operand(
                    context,
                    program,
                    operand,
                    clone_rows(&segment_input),
                    metrics,
                    is_interrupted,
                )?;
                result = union_rows(
                    context,
                    result,
                    right,
                    connector != QueryConnector::UnionAll,
                )?;
            }
        }
        previous_operand = operand;
    }
    Ok(result)
}

fn execute_operand(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    operand: &AstNode,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    match operand.kind {
        AstKind::SingleQuery => {
            execute_single(context, program, operand, input, metrics, is_interrupted)
        }
        AstKind::Subquery(_) => {
            let body = required_query_body(operand, "braced query is missing its body")?;
            execute_query_body(context, program, body, input, metrics, is_interrupted)
        }
        _ => Err(QueryError::internal(
            "composed program contains a non-executable operand",
        )),
    }
}

fn execute_conditional(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    query: &AstNode,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let input_columns = input.columns.clone();
    let mut output: Option<RowSet> = None;
    for row in input.rows {
        check_interrupted(is_interrupted)?;
        let selected = {
            let snapshot = context.staged_snapshot()?;
            select_branch(query, |expression| {
                expression::predicate(expression::evaluate(
                    &compile_expression(expression)?,
                    &snapshot,
                    &row,
                    context.params,
                )?)
            })?
        };
        let Some(branch) = selected else {
            continue;
        };
        let branch_result =
            execute_conditional_branch(context, program, branch, row, metrics, is_interrupted)?;
        output = Some(match output {
            Some(current) => union_rows(context, current, branch_result, false)?,
            None => branch_result,
        });
    }
    Ok(output.unwrap_or(RowSet {
        columns: crate::query::completeness::infer_columns(query, &input_columns, &program.source)?,
        rows: Vec::new(),
    }))
}

fn execute_conditional_branch(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    branch: &AstNode,
    row: BindingRow,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let input = RowSet {
        columns: row.order.clone(),
        rows: vec![row],
    };
    match conditional_branch_query(branch)? {
        ConditionalBranchQuery::Body(body) => {
            execute_query_body(context, program, body, input, metrics, is_interrupted)
        }
        ConditionalBranchQuery::Composed(query) => {
            execute_composed(context, program, query, input, metrics, is_interrupted)
        }
    }
}

fn execute_single(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    single: &AstNode,
    rows: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    execute_single_from(context, program, single, 0, rows, metrics, is_interrupted)
}

#[allow(
    clippy::too_many_arguments,
    reason = "streaming LOAD CSV keeps the mutation context and clause tail explicit"
)]
fn execute_single_from(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    single: &AstNode,
    start: usize,
    mut rows: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    for (index, clause) in single.children.iter().enumerate().skip(start) {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        check_interrupted(is_interrupted)?;
        if kind == ClauseKind::LoadCsv {
            return execute_load_csv_tail(
                context,
                program,
                single,
                index,
                rows,
                metrics,
                is_interrupted,
            );
        }
        rows = execute_single_clause(
            context,
            program,
            clause,
            kind,
            rows,
            metrics,
            is_interrupted,
        )?;
    }
    Ok(rows)
}

#[allow(
    clippy::too_many_arguments,
    reason = "the clause dispatcher keeps mutation state and cancellation explicit"
)]
fn execute_single_clause(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    kind: ClauseKind,
    rows: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    match kind {
        ClauseKind::Create
        | ClauseKind::Insert
        | ClauseKind::Set
        | ClauseKind::Remove
        | ClauseKind::Delete
        | ClauseKind::DetachDelete
        | ClauseKind::Merge => {
            execute_basic_write_clause(context, program, clause, kind, rows, is_interrupted)
        }
        ClauseKind::Foreach => {
            execute_foreach(context, program, clause, rows, metrics, is_interrupted)
        }
        ClauseKind::Call => execute_call(context, program, clause, rows, metrics, is_interrupted),
        ClauseKind::With => {
            let projected = execute_read(context, program, clause, rows, metrics, is_interrupted)?;
            Ok(restore_global_bindings(projected, &context.global_bindings))
        }
        _ => execute_read(context, program, clause, rows, metrics, is_interrupted),
    }
}

fn execute_basic_write_clause(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    kind: ClauseKind,
    rows: RowSet,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let columns = rows.columns;
    let rows = match kind {
        ClauseKind::Create | ClauseKind::Insert => execute_create_clause(
            context,
            &lower_write_pattern(clause)?,
            rows.rows,
            is_interrupted,
        )?,
        ClauseKind::Set => execute_set_clause(
            context,
            &lower_set_items(clause)?,
            rows.rows,
            is_interrupted,
        )?,
        ClauseKind::Remove => execute_remove_clause(
            context,
            &lower_remove_items(clause)?,
            rows.rows,
            is_interrupted,
        )?,
        ClauseKind::Delete | ClauseKind::DetachDelete => execute_delete_clause(
            context,
            &lower_delete_expressions(clause)?,
            kind == ClauseKind::DetachDelete,
            rows.rows,
            is_interrupted,
        )?,
        ClauseKind::Merge => {
            execute_merge_clause(context, &lower_merge(clause)?, rows.rows, is_interrupted)?
        }
        _ => {
            return Err(QueryError::internal(
                "non-write clause reached write dispatcher",
            ));
        }
    };
    let columns = if matches!(
        kind,
        ClauseKind::Create | ClauseKind::Insert | ClauseKind::Merge
    ) {
        inferred_clause_columns(clause, &columns, &program.source)?
    } else {
        columns
    };
    Ok(RowSet { columns, rows })
}

fn inferred_clause_columns(
    clause: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    crate::query::completeness::infer_clause_columns(clause, input, source)
}

fn execute_read(
    context: &MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    rows: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let snapshot = context.staged_snapshot()?;
    let graph_view = context.graph_view()?;
    execute_read_clause(
        context.connection,
        snapshot,
        &graph_view,
        context.params,
        &program.source,
        clause,
        rows,
        metrics,
        is_interrupted,
    )
}

fn execute_call(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let Some(subquery) = clause
        .children
        .iter()
        .find(|node| node.kind == AstKind::Subquery(SubqueryKind::Call))
    else {
        return execute_read(context, program, clause, input, metrics, is_interrupted);
    };
    let body = required_query_body(subquery, "CALL subquery is missing its query body")?;
    execute_call_subquery_rows(
        context,
        program,
        subquery,
        body,
        input,
        metrics,
        is_interrupted,
    )
}

fn merge_call_result(
    outer: BindingRow,
    inner: RowSet,
    returns_rows: bool,
    columns: &mut Vec<String>,
    output: &mut Vec<BindingRow>,
) {
    for column in &inner.columns {
        if !columns.contains(column) {
            columns.push(column.clone());
        }
    }
    if !returns_rows {
        output.push(outer);
        return;
    }
    for inner_row in inner.rows {
        let mut combined = outer.clone();
        for column in &inner.columns {
            combined.insert(
                column.clone(),
                inner_row
                    .values
                    .get(column)
                    .cloned()
                    .unwrap_or(BindingValue::Null),
            );
        }
        if inner_row.load_csv_context.is_some() {
            combined
                .load_csv_context
                .clone_from(&inner_row.load_csv_context);
        }
        output.push(combined);
    }
}

fn execute_call_body_with_importing_with(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    body: &AstNode,
    outer: &BindingRow,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let query = executable_query(body, true)?;
    execute_call_query_with_importing_with(context, program, query, outer, metrics, is_interrupted)
}

fn execute_call_query_with_importing_with(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    query: &AstNode,
    outer: &BindingRow,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    match query.kind {
        AstKind::SingleQuery => execute_single(
            context,
            program,
            query,
            importing_with_seed(query, outer),
            metrics,
            is_interrupted,
        ),
        AstKind::Subquery(SubqueryKind::Braced) => {
            let body = required_query_body(query, "braced query is missing its body")?;
            execute_call_body_with_importing_with(
                context,
                program,
                body,
                outer,
                metrics,
                is_interrupted,
            )
        }
        AstKind::ComposedQuery => {
            execute_call_composed(context, program, query, outer, metrics, is_interrupted)
        }
        AstKind::ConditionalQuery => execute_conditional(
            context,
            program,
            query,
            RowSet::seed(),
            metrics,
            is_interrupted,
        ),
        _ => Err(QueryError::internal("invalid CALL subquery query body")),
    }
}

fn execute_call_composed(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    query: &AstNode,
    outer: &BindingRow,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let parts = composed_query_parts(query)?;
    let mut result = execute_call_query_with_importing_with(
        context,
        program,
        parts.first,
        outer,
        metrics,
        is_interrupted,
    )?;
    let mut segment_input = None;
    let mut previous_operand = parts.first;
    for (connector, operand) in parts.rest {
        check_interrupted(is_interrupted)?;
        match connector {
            QueryConnector::Next => {
                let next_input = rows_for_next(previous_operand, result);
                result = execute_operand(
                    context,
                    program,
                    operand,
                    clone_rows(&next_input),
                    metrics,
                    is_interrupted,
                )?;
                segment_input = Some(next_input);
            }
            QueryConnector::Union | QueryConnector::UnionAll | QueryConnector::UnionDistinct => {
                let right = if let Some(input) = &segment_input {
                    execute_operand(
                        context,
                        program,
                        operand,
                        clone_rows(input),
                        metrics,
                        is_interrupted,
                    )?
                } else {
                    execute_call_query_with_importing_with(
                        context,
                        program,
                        operand,
                        outer,
                        metrics,
                        is_interrupted,
                    )?
                };
                result = union_rows(
                    context,
                    result,
                    right,
                    connector != QueryConnector::UnionAll,
                )?;
            }
        }
        previous_operand = operand;
    }
    Ok(result)
}

fn importing_with_seed(single: &AstNode, outer: &BindingRow) -> RowSet {
    let columns = crate::query::completeness::importing_with_columns(single, &outer.order);
    let row = project_bindings(outer, &columns);
    RowSet {
        columns,
        rows: vec![row],
    }
}

fn execute_foreach(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let spec = lower_foreach(clause)?;
    let output_rows = input.rows;
    for outer in &output_rows {
        for value in foreach_values(context, &spec.expression, outer)? {
            check_interrupted(is_interrupted)?;
            execute_foreach_value(
                context,
                program,
                &spec,
                outer,
                value,
                metrics,
                is_interrupted,
            )?;
        }
    }
    Ok(RowSet {
        columns: input.columns,
        rows: output_rows,
    })
}

struct ForeachSpec {
    variable: String,
    expression: expression::Expr,
    updates: Vec<AstNode>,
}

fn lower_foreach(clause: &AstNode) -> QueryResult<ForeachSpec> {
    let variable = clause
        .children
        .iter()
        .find(|node| node.kind == AstKind::BindingVariable)
        .and_then(|node| node.text.clone())
        .ok_or_else(|| QueryError::semantic("FOREACH is missing its loop variable"))?;
    let expression = compile_expression(first_surface_expression(clause)?)?;
    let updates = clause
        .children
        .iter()
        .filter(|node| matches!(node.kind, AstKind::Clause(_)))
        .cloned()
        .collect();
    Ok(ForeachSpec {
        variable,
        expression,
        updates,
    })
}

fn foreach_values(
    context: &MutationContext<'_, '_>,
    expression: &expression::Expr,
    outer: &BindingRow,
) -> QueryResult<Vec<Value>> {
    let snapshot = context.staged_snapshot()?;
    match expression::evaluate(expression, &snapshot, outer, context.params)? {
        Value::Null => Ok(Vec::new()),
        Value::List(values) => Ok(values),
        _ => Err(QueryError::new(
            QueryErrorKind::Type,
            "FOREACH input must be a List or null",
        )),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "one FOREACH iteration owns its mutation context and cancellation boundary"
)]
fn execute_foreach_value(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    spec: &ForeachSpec,
    outer: &BindingRow,
    value: Value,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let snapshot = context.staged_snapshot()?;
    let mut local = outer.clone();
    local.insert(
        spec.variable.clone(),
        expression::binding_from_value(&snapshot, value)?,
    );
    let mut rows = RowSet {
        columns: local.order.clone(),
        rows: vec![local],
    };
    for update in &spec.updates {
        validate_foreach_update(update)?;
        rows = execute_single(
            context,
            program,
            &AstNode {
                kind: AstKind::SingleQuery,
                span: update.span,
                text: None,
                children: vec![update.clone()],
            },
            rows,
            metrics,
            is_interrupted,
        )?;
    }
    Ok(())
}

fn validate_foreach_update(update: &AstNode) -> QueryResult<()> {
    let AstKind::Clause(kind) = update.kind else {
        return Err(QueryError::internal("FOREACH update is not a clause"));
    };
    if matches!(
        kind,
        ClauseKind::Create
            | ClauseKind::Insert
            | ClauseKind::Merge
            | ClauseKind::Set
            | ClauseKind::Remove
            | ClauseKind::Delete
            | ClauseKind::DetachDelete
            | ClauseKind::Foreach
    ) {
        Ok(())
    } else {
        Err(QueryError::semantic(
            "FOREACH accepts only updating clauses",
        ))
    }
}

fn union_rows(
    context: &MutationContext<'_, '_>,
    left: RowSet,
    right: RowSet,
    distinct: bool,
) -> QueryResult<RowSet> {
    let mut left = merge_union_rows(left, right)?;
    if distinct {
        let snapshot = context.staged_snapshot()?;
        left.rows = distinct_bindings(left.rows, &left.columns, &snapshot)?;
    }
    Ok(left)
}

fn finish_program(
    context: MutationContext<'_, '_>,
    program: &PreparedProgram,
    result: RowSet,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<WriteOutcome> {
    check_interrupted(is_interrupted)?;
    let final_layer = context.delta.layer()?;
    let counters = context.delta.counters()?;
    let final_snapshot =
        Snapshot::resolve_with_layer(context.connection, context.base_commit, &final_layer)?;
    crate::query::schema::validate_layer_against_commit_schema(
        context.connection,
        context.base_commit,
        &final_snapshot,
        &final_layer,
    )?;
    let rows = if program.public_result {
        validate_executed_columns(&program.columns, &result.columns)?;
        materialize_binding_rows_to_spill(
            &final_snapshot,
            &result.columns,
            result.rows,
            is_interrupted,
        )?
    } else {
        None
    };
    check_interrupted(is_interrupted)?;
    let options = program
        .write_options
        .as_ref()
        .ok_or_else(|| QueryError::internal("mutating program is missing write options"))?;
    let metadata = CommitMetadata {
        author: options.author.clone(),
        message: options.message.clone(),
        committed_at: now_micros()?,
    };
    let commit = storage::commit_layer(
        context.connection,
        &options.branch,
        context.base_commit,
        None,
        &final_layer,
        &metadata,
    )?;
    Ok(WriteOutcome {
        rows,
        commit,
        counters,
    })
}

fn clone_rows(input: &RowSet) -> RowSet {
    RowSet {
        columns: input.columns.clone(),
        rows: input.rows.clone(),
    }
}

fn first_surface_expression(node: &AstNode) -> QueryResult<&AstNode> {
    node.children
        .iter()
        .find(|child| matches!(child.kind, AstKind::Expression(_)))
        .or_else(|| {
            node.descendants()
                .skip(1)
                .find(|child| matches!(child.kind, AstKind::Expression(_)))
        })
        .ok_or_else(|| QueryError::semantic("clause is missing its expression"))
}
