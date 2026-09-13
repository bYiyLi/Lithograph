use std::collections::BTreeMap;
use std::thread;
use std::time::{Duration, Instant};

use rusqlite::Connection;

use crate::cypher::{
    AstKind, AstNode, ClauseKind, TransactionDisjointKind, TransactionErrorKind, Value,
};
use crate::storage::{self, HashId, Snapshot};

use super::completeness::execute::{
    ConditionalBranchQuery, RowSet, conditional_branch_query, execute_read_clause, rows_for_next,
    select_conditional_branch,
};
use super::completeness::{
    PreparedProgram, TransactionProgramOptions, composed_query_parts, executable_query,
    infer_clause_columns, infer_columns, projection_requires_global_input,
};
use super::expression::{self, BindingRow, BindingValue, compile_expression, surface_expressions};
use super::graph::ResolvedGraphView;
use super::ingestion::{
    CompiledLoadCsv, compile_load_csv, evaluate_csv_source, stream_csv_binding_rows,
};
use super::mutation::{
    MutationCounters, TransactionBatchOutcome, TransactionMutationContext,
    execute_program_suffix_transaction, execute_transaction_batch,
};
use super::spill::{BindingSpill, open_spill_connection};
use super::{QueryCounters, QueryError, QueryErrorKind, QueryMetrics, QueryResult};

pub(crate) struct TransactionProgramOutcome {
    pub(crate) rows: Vec<Vec<Value>>,
    pub(crate) commit: HashId,
    pub(crate) counters: QueryCounters,
}

struct TransactionRuntime<'a> {
    connection: &'a Connection,
    program: &'a PreparedProgram,
    options: &'a TransactionProgramOptions,
    params: &'a BTreeMap<String, Value>,
    metrics: &'a mut QueryMetrics,
    is_interrupted: &'a dyn Fn() -> bool,
    commit: HashId,
    counters: QueryCounters,
    next_batch_id: u64,
}

impl TransactionRuntime<'_> {
    fn mutation_context(&mut self, base_commit: HashId) -> TransactionMutationContext<'_> {
        TransactionMutationContext {
            connection: self.connection,
            program: self.program,
            base_commit,
            branch: &self.options.branch,
            graph_view: &self.options.graph_view,
            author: self.options.author.as_deref(),
            message: self.options.message.as_deref(),
            params: self.params,
            metrics: &mut *self.metrics,
            is_interrupted: self.is_interrupted,
        }
    }
}

struct TransactionSpec {
    batch_size: Option<expression::Expr>,
    concurrency: Option<expression::Expr>,
    disjoint: Option<(TransactionDisjointKind, Vec<expression::Expr>)>,
    error: TransactionErrorKind,
    retry_seconds: Option<expression::Expr>,
    retry_fallback: TransactionErrorKind,
    status_alias: Option<String>,
}

struct BatchFailure {
    error: QueryError,
    transaction_id: String,
}

struct TransactionCallResult {
    rows: RowSet,
    broke: bool,
}

struct CsvTransactionPipeline<'a> {
    single: &'a AstNode,
    csv_index: usize,
    transaction_index: usize,
    transaction_clause: &'a AstNode,
    spec: TransactionSpec,
    csv: CompiledLoadCsv,
}

struct CsvBatchState {
    pending_rows: Vec<BindingRow>,
    pending_columns: Vec<String>,
    transaction_rows: Vec<BindingRow>,
    transaction_columns: Vec<String>,
    source_guards: Vec<tempfile::TempPath>,
    broken: bool,
}

pub(crate) fn execute_transaction_program(
    connection: &Connection,
    program: &PreparedProgram,
    initial_commit: HashId,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<TransactionProgramOutcome> {
    if !connection.is_autocommit() {
        return Err(QueryError::transaction_boundary_required(
            "CALL subqueries IN TRANSACTIONS require Native autocommit execution",
        ));
    }
    let options = program.transaction_options.as_ref().ok_or_else(|| {
        QueryError::internal("transaction program is missing transaction options")
    })?;
    let mut runtime = TransactionRuntime {
        connection,
        program,
        options,
        params,
        metrics,
        is_interrupted,
        commit: initial_commit,
        counters: QueryCounters::default(),
        next_batch_id: 1,
    };
    let result = execute_transaction_query(&mut runtime, &program.root, RowSet::seed())?;
    super::completeness::execute::validate_executed_columns(&program.columns, &result.columns)?;
    let snapshot = Snapshot::resolve(connection, runtime.commit)?;
    let rows = materialize_rows(&snapshot, result)?;
    Ok(TransactionProgramOutcome {
        rows,
        commit: runtime.commit,
        counters: runtime.counters,
    })
}

fn execute_transaction_query(
    runtime: &mut TransactionRuntime<'_>,
    body: &AstNode,
    input: RowSet,
) -> QueryResult<RowSet> {
    let query = executable_query(body, true)?;
    match query.kind {
        AstKind::SingleQuery => execute_single_from(runtime, query, 0, input),
        AstKind::ComposedQuery => execute_transaction_composed(runtime, query, input),
        AstKind::ConditionalQuery => execute_transaction_conditional(runtime, query, input),
        _ => Err(QueryError::internal(
            "transaction query resolved to an invalid executable node",
        )),
    }
}

fn execute_transaction_composed(
    runtime: &mut TransactionRuntime<'_>,
    query: &AstNode,
    input: RowSet,
) -> QueryResult<RowSet> {
    let parts = composed_query_parts(query)?;
    let mut result = execute_transaction_operand(runtime, parts.first, clone_row_set(&input))?;
    let mut previous = parts.first;
    for (connector, operand) in parts.rest {
        if connector != crate::cypher::QueryConnector::Next {
            return Err(QueryError::semantic(
                "CALL subqueries IN TRANSACTIONS are not supported inside UNION",
            ));
        }
        let next_input = rows_for_next(previous, result);
        result = execute_transaction_operand(runtime, operand, next_input)?;
        previous = operand;
    }
    Ok(result)
}

fn execute_transaction_operand(
    runtime: &mut TransactionRuntime<'_>,
    operand: &AstNode,
    input: RowSet,
) -> QueryResult<RowSet> {
    match operand.kind {
        AstKind::SingleQuery => execute_single_from(runtime, operand, 0, input),
        AstKind::Subquery(crate::cypher::SubqueryKind::Braced) => {
            let body = operand
                .children
                .iter()
                .find(|node| node.kind == AstKind::QueryBody)
                .ok_or_else(|| QueryError::semantic("braced query is missing its body"))?;
            execute_transaction_query(runtime, body, input)
        }
        _ => Err(QueryError::semantic(
            "transaction query composition contains an unsupported operand",
        )),
    }
}

fn clone_row_set(input: &RowSet) -> RowSet {
    RowSet {
        columns: input.columns.clone(),
        rows: input.rows.clone(),
    }
}

fn execute_transaction_conditional(
    runtime: &mut TransactionRuntime<'_>,
    query: &AstNode,
    input: RowSet,
) -> QueryResult<RowSet> {
    let input_columns = input.columns.clone();
    let mut output: Option<RowSet> = None;
    for row in input.rows {
        let Some(branch) = select_conditional_branch(query, |expression| {
            expression::predicate(evaluate_at_commit(
                runtime,
                &compile_expression(expression)?,
                &row,
            )?)
        })?
        else {
            continue;
        };
        let branch_input = RowSet {
            columns: row.order.clone(),
            rows: vec![row],
        };
        let result = match conditional_branch_query(branch)? {
            ConditionalBranchQuery::Body(body) => {
                execute_transaction_query(runtime, body, branch_input)?
            }
            ConditionalBranchQuery::Composed(composed) => {
                execute_transaction_composed(runtime, composed, branch_input)?
            }
        };
        output = Some(match output {
            Some(mut current) => {
                if current.columns != result.columns {
                    return Err(QueryError::internal(
                        "conditional transaction branches produced different columns",
                    ));
                }
                current.rows.extend(result.rows);
                current
            }
            None => result,
        });
    }
    if let Some(output) = output {
        return Ok(output);
    }
    Ok(RowSet {
        columns: infer_columns(query, &input_columns, &runtime.program.source)?,
        rows: Vec::new(),
    })
}

fn materialize_rows(snapshot: &Snapshot<'_>, result: RowSet) -> QueryResult<Vec<Vec<Value>>> {
    result
        .rows
        .into_iter()
        .map(|row| {
            result
                .columns
                .iter()
                .map(|column| {
                    expression::binding_value(
                        snapshot,
                        row.values.get(column).unwrap_or(&BindingValue::Null),
                    )
                })
                .collect::<QueryResult<Vec<_>>>()
        })
        .collect()
}

fn execute_single_from(
    runtime: &mut TransactionRuntime<'_>,
    single: &AstNode,
    start: usize,
    mut rows: RowSet,
) -> QueryResult<RowSet> {
    for (index, clause) in single.children.iter().enumerate().skip(start) {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        check_interrupted(runtime.is_interrupted)?;
        if kind == ClauseKind::Call && transaction_subquery(clause).is_some() {
            rows = execute_transaction_call(runtime, clause, rows)?;
            continue;
        }
        if is_outer_write_clause(kind) {
            return execute_outer_write_suffix(runtime, single, index, rows);
        }
        if kind == ClauseKind::LoadCsv && find_next_transaction_call(single, index + 1).is_some() {
            return execute_streaming_load_csv(runtime, single, index, clause, rows);
        }
        rows = execute_read_clause_at_commit(runtime, clause, rows)?;
    }
    Ok(rows)
}

fn execute_outer_write_suffix(
    runtime: &mut TransactionRuntime<'_>,
    single: &AstNode,
    start: usize,
    input: RowSet,
) -> QueryResult<RowSet> {
    let base_commit = storage::branch_head(runtime.connection, &runtime.options.branch)?;
    runtime.commit = base_commit;
    let mut transaction = runtime.mutation_context(base_commit);
    let outcome = execute_program_suffix_transaction(&mut transaction, single, start, input)?;
    runtime.commit = outcome.commit;
    add_mutation_counters(&mut runtime.counters, &outcome.counters);
    Ok(outcome.rows)
}

fn execute_read_clause_at_commit(
    runtime: &mut TransactionRuntime<'_>,
    clause: &AstNode,
    input: RowSet,
) -> QueryResult<RowSet> {
    let snapshot = Snapshot::resolve(runtime.connection, runtime.commit)?;
    let graph_view = ResolvedGraphView::resolve(runtime.connection, &runtime.options.graph_view)?;
    execute_read_clause(
        runtime.connection,
        snapshot,
        &graph_view,
        runtime.params,
        &runtime.program.source,
        clause,
        input,
        runtime.metrics,
        runtime.is_interrupted,
    )
}

fn is_outer_write_clause(kind: ClauseKind) -> bool {
    matches!(
        kind,
        ClauseKind::Create
            | ClauseKind::Insert
            | ClauseKind::Merge
            | ClauseKind::Set
            | ClauseKind::Remove
            | ClauseKind::Delete
            | ClauseKind::DetachDelete
            | ClauseKind::Foreach
    )
}

fn transaction_subquery(clause: &AstNode) -> Option<&AstNode> {
    clause.children.iter().find(|node| {
        node.kind == AstKind::Subquery(crate::cypher::SubqueryKind::Call)
            && node
                .children
                .iter()
                .any(|child| child.kind == AstKind::TransactionSubclause)
    })
}

fn find_next_transaction_call(single: &AstNode, start: usize) -> Option<usize> {
    single
        .children
        .iter()
        .enumerate()
        .skip(start)
        .find_map(|(index, node)| {
            (node.kind == AstKind::Clause(ClauseKind::Call) && transaction_subquery(node).is_some())
                .then_some(index)
        })
}

fn check_interrupted(is_interrupted: &dyn Fn() -> bool) -> QueryResult<()> {
    if is_interrupted() {
        Err(QueryError::interrupted())
    } else {
        Ok(())
    }
}

fn transaction_spec(subquery: &AstNode) -> QueryResult<TransactionSpec> {
    let transaction = subquery
        .children
        .iter()
        .find(|node| node.kind == AstKind::TransactionSubclause)
        .ok_or_else(|| {
            QueryError::internal("transaction CALL is missing its transaction clause")
        })?;
    let batch_size = transaction
        .descendants()
        .find(|node| node.kind == AstKind::TransactionBatch)
        .and_then(|node| surface_expressions(node).into_iter().next())
        .map(compile_expression)
        .transpose()?;
    let concurrency = transaction
        .descendants()
        .find(|node| node.kind == AstKind::TransactionConcurrent)
        .and_then(|node| surface_expressions(node).into_iter().next())
        .map(compile_expression)
        .transpose()?;
    let disjoint = transaction
        .descendants()
        .find_map(|node| match node.kind {
            AstKind::TransactionDisjoint(kind) => Some((kind, node)),
            _ => None,
        })
        .map(|(kind, node)| {
            let expressions = if kind == TransactionDisjointKind::Explicit {
                surface_expressions(node)
                    .into_iter()
                    .map(compile_expression)
                    .collect::<QueryResult<Vec<_>>>()?
            } else {
                Vec::new()
            };
            Ok::<_, QueryError>((kind, expressions))
        })
        .transpose()?;
    let error_node = transaction
        .descendants()
        .find(|node| matches!(node.kind, AstKind::TransactionError(_)));
    let error = error_node
        .and_then(|node| match node.kind {
            AstKind::TransactionError(kind) => Some(kind),
            _ => None,
        })
        .unwrap_or(TransactionErrorKind::Fail);
    let retry_seconds = error_node
        .filter(|_| error == TransactionErrorKind::Retry)
        .and_then(|node| surface_expressions(node).into_iter().next())
        .map(compile_expression)
        .transpose()?;
    let retry_fallback = error_node
        .and_then(|node| {
            node.descendants().find_map(|child| match child.kind {
                AstKind::TransactionRetryFallback(kind) => Some(kind),
                _ => None,
            })
        })
        .unwrap_or(TransactionErrorKind::Fail);
    let status_alias = transaction
        .descendants()
        .find(|node| node.kind == AstKind::TransactionStatusBinding)
        .and_then(|node| node.text.as_deref())
        .map(crate::cypher::unescape_identifier);
    let effective_error = if error == TransactionErrorKind::Retry {
        retry_fallback
    } else {
        error
    };
    if status_alias.is_some() && effective_error == TransactionErrorKind::Fail {
        return Err(QueryError::semantic(
            "REPORT STATUS requires ON ERROR CONTINUE or ON ERROR BREAK",
        ));
    }
    Ok(TransactionSpec {
        batch_size,
        concurrency,
        disjoint,
        error,
        retry_seconds,
        retry_fallback,
        status_alias,
    })
}

fn execute_transaction_call(
    runtime: &mut TransactionRuntime<'_>,
    clause: &AstNode,
    input: RowSet,
) -> QueryResult<RowSet> {
    Ok(execute_transaction_call_controlled(runtime, clause, input)?.rows)
}

fn execute_transaction_call_controlled(
    runtime: &mut TransactionRuntime<'_>,
    clause: &AstNode,
    input: RowSet,
) -> QueryResult<TransactionCallResult> {
    let subquery = transaction_subquery(clause)
        .ok_or_else(|| QueryError::internal("transaction CALL is missing its subquery"))?;
    let spec = transaction_spec(subquery)?;
    let columns = call_output_columns(clause, &input.columns, &runtime.program.source)?;
    if input.rows.is_empty() {
        return Ok(TransactionCallResult {
            rows: RowSet {
                columns,
                rows: Vec::new(),
            },
            broke: false,
        });
    }
    validate_concurrency(runtime, &spec, &input.rows[0])?;
    let batch_size = resolve_batch_size(runtime, &spec, &input.rows[0])?;
    validate_disjoint_expressions(runtime, &spec, &input.rows)?;
    execute_transaction_batches(runtime, subquery, spec, columns, input, batch_size)
}

fn execute_transaction_batches(
    runtime: &mut TransactionRuntime<'_>,
    subquery: &AstNode,
    spec: TransactionSpec,
    columns: Vec<String>,
    input: RowSet,
    batch_size: usize,
) -> QueryResult<TransactionCallResult> {
    let mut output = Vec::new();
    let input_columns = input.columns;
    let all_rows = input.rows;
    let mut start = 0;
    let mut broke = false;
    while start < all_rows.len() {
        check_interrupted(runtime.is_interrupted)?;
        let end = start.saturating_add(batch_size).min(all_rows.len());
        let batch_rows = all_rows[start..end].to_vec();
        let transaction_id = next_transaction_id(runtime);
        let batch = RowSet {
            columns: input_columns.clone(),
            rows: batch_rows.clone(),
        };
        match execute_batch_with_retry(runtime, subquery, batch, &spec, &transaction_id)? {
            Ok(successful) => {
                append_successful_batch(runtime, &spec, &transaction_id, successful, &mut output)
            }
            Err(failure) => {
                if append_failed_batch(
                    &spec,
                    failure,
                    &batch_rows,
                    &all_rows[end..],
                    &columns,
                    &mut output,
                )? {
                    broke = true;
                    break;
                }
            }
        }
        start = end;
    }
    Ok(TransactionCallResult {
        rows: RowSet {
            columns,
            rows: output,
        },
        broke,
    })
}

fn append_successful_batch(
    runtime: &mut TransactionRuntime<'_>,
    spec: &TransactionSpec,
    transaction_id: &str,
    mut successful: TransactionBatchOutcome,
    output: &mut Vec<BindingRow>,
) {
    runtime.commit = successful.commit;
    add_mutation_counters(&mut runtime.counters, &successful.counters);
    if let Some(alias) = &spec.status_alias {
        let status = transaction_status(true, true, Some(transaction_id), None);
        for row in &mut successful.rows.rows {
            row.insert(alias.clone(), BindingValue::Scalar(status.clone()));
        }
    }
    output.extend(successful.rows.rows);
}

fn append_failed_batch(
    spec: &TransactionSpec,
    failure: BatchFailure,
    batch_rows: &[BindingRow],
    remaining_rows: &[BindingRow],
    columns: &[String],
    output: &mut Vec<BindingRow>,
) -> QueryResult<bool> {
    let effective = effective_error_mode(spec);
    if effective == TransactionErrorKind::Fail {
        return Err(failure.error);
    }
    output.extend(failed_rows(
        batch_rows,
        columns,
        spec.status_alias.as_deref(),
        transaction_status(
            true,
            false,
            Some(&failure.transaction_id),
            Some(&failure.error.message),
        ),
    ));
    if effective != TransactionErrorKind::Break {
        return Ok(false);
    }
    output.extend(failed_rows(
        remaining_rows,
        columns,
        spec.status_alias.as_deref(),
        transaction_status(false, false, None, None),
    ));
    Ok(true)
}

fn call_output_columns(
    clause: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    infer_clause_columns(clause, input, source)
}

fn resolve_batch_size(
    runtime: &TransactionRuntime<'_>,
    spec: &TransactionSpec,
    row: &BindingRow,
) -> QueryResult<usize> {
    let Some(expression) = &spec.batch_size else {
        return Ok(1_000);
    };
    let value = evaluate_at_commit(runtime, expression, row)?;
    let Value::Integer(value) = value else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "IN TRANSACTIONS batch size must be an Integer",
        ));
    };
    usize::try_from(value)
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| QueryError::invalid_argument("IN TRANSACTIONS batch size must be positive"))
}

fn validate_concurrency(
    runtime: &TransactionRuntime<'_>,
    spec: &TransactionSpec,
    row: &BindingRow,
) -> QueryResult<()> {
    let Some(expression) = &spec.concurrency else {
        return Ok(());
    };
    let value = evaluate_at_commit(runtime, expression, row)?;
    let Value::Integer(value) = value else {
        return Err(QueryError::new(
            QueryErrorKind::Type,
            "CONCURRENT TRANSACTIONS concurrency must be an Integer",
        ));
    };
    if value == 0 {
        return Err(QueryError::invalid_argument(
            "CONCURRENT TRANSACTIONS concurrency cannot be zero",
        ));
    }
    Ok(())
}

fn validate_disjoint_expressions(
    runtime: &TransactionRuntime<'_>,
    spec: &TransactionSpec,
    rows: &[BindingRow],
) -> QueryResult<()> {
    let Some((TransactionDisjointKind::Explicit, expressions)) = &spec.disjoint else {
        return Ok(());
    };
    for row in rows {
        for expression in expressions {
            let _ = evaluate_at_commit(runtime, expression, row)?;
        }
    }
    Ok(())
}

fn evaluate_at_commit(
    runtime: &TransactionRuntime<'_>,
    expression: &expression::Expr,
    row: &BindingRow,
) -> QueryResult<Value> {
    let snapshot = Snapshot::resolve(runtime.connection, runtime.commit)?;
    expression::evaluate(expression, &snapshot, row, runtime.params)
}

fn execute_batch_with_retry(
    runtime: &mut TransactionRuntime<'_>,
    subquery: &AstNode,
    batch: RowSet,
    spec: &TransactionSpec,
    transaction_id: &str,
) -> QueryResult<Result<TransactionBatchOutcome, BatchFailure>> {
    let retry_seconds = resolve_retry_seconds(runtime, spec, batch.rows.first())?;
    let started = Instant::now();
    let mut attempt = 0_u32;
    let mut delay = Duration::from_millis(1);
    loop {
        check_interrupted(runtime.is_interrupted)?;
        let base_commit = storage::branch_head(runtime.connection, &runtime.options.branch)?;
        runtime.commit = base_commit;
        let mut transaction = runtime.mutation_context(base_commit);
        let result = execute_transaction_batch(
            &mut transaction,
            subquery,
            RowSet {
                columns: batch.columns.clone(),
                rows: batch.rows.clone(),
            },
        );
        match result {
            Ok(outcome) => return Ok(Ok(outcome)),
            Err(error) => {
                let should_retry = spec.error == TransactionErrorKind::Retry
                    && is_transient(&error)
                    && (attempt == 0
                        || retry_seconds.is_some_and(|seconds| {
                            started.elapsed() < Duration::from_secs_f64(seconds.max(0.0))
                        })
                        || retry_seconds.is_none());
                if !should_retry {
                    return Ok(Err(BatchFailure {
                        error,
                        transaction_id: transaction_id.to_owned(),
                    }));
                }
                attempt = attempt.saturating_add(1);
                if attempt > 1
                    && retry_seconds.is_some_and(|seconds| {
                        started.elapsed() >= Duration::from_secs_f64(seconds.max(0.0))
                    })
                {
                    return Ok(Err(BatchFailure {
                        error,
                        transaction_id: transaction_id.to_owned(),
                    }));
                }
                thread::sleep(delay);
                delay = (delay * 2).min(Duration::from_millis(100));
            }
        }
    }
}

fn resolve_retry_seconds(
    runtime: &TransactionRuntime<'_>,
    spec: &TransactionSpec,
    row: Option<&BindingRow>,
) -> QueryResult<Option<f64>> {
    if spec.error != TransactionErrorKind::Retry {
        return Ok(None);
    }
    let Some(expression) = &spec.retry_seconds else {
        return Ok(Some(30.0));
    };
    let row = row.ok_or_else(|| QueryError::internal("retry batch has no input row"))?;
    let value = evaluate_at_commit(runtime, expression, row)?;
    let seconds = match value {
        Value::Integer(value) => value as f64,
        Value::Float(value) => value,
        _ => {
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "ON ERROR RETRY duration must be numeric",
            ));
        }
    };
    if seconds.is_finite() && seconds >= 0.0 {
        Ok(Some(seconds))
    } else {
        Err(QueryError::invalid_argument(
            "ON ERROR RETRY duration must be non-negative and finite",
        ))
    }
}

fn is_transient(error: &QueryError) -> bool {
    error.kind == QueryErrorKind::BranchHeadMoved
        || matches!(
            error.sqlite_code,
            Some(rusqlite::ffi::SQLITE_BUSY | rusqlite::ffi::SQLITE_LOCKED)
        )
}

fn effective_error_mode(spec: &TransactionSpec) -> TransactionErrorKind {
    if spec.error == TransactionErrorKind::Retry {
        spec.retry_fallback
    } else {
        spec.error
    }
}

fn next_transaction_id(runtime: &mut TransactionRuntime<'_>) -> String {
    let id = runtime.next_batch_id;
    runtime.next_batch_id = runtime.next_batch_id.saturating_add(1);
    format!("lithograph-transaction-{id}")
}

fn transaction_status(
    started: bool,
    committed: bool,
    transaction_id: Option<&str>,
    error_message: Option<&str>,
) -> Value {
    Value::Map(BTreeMap::from([
        ("started".to_owned(), Value::Boolean(started)),
        ("committed".to_owned(), Value::Boolean(committed)),
        (
            "transactionId".to_owned(),
            transaction_id.map_or(Value::Null, |value| Value::String(value.to_owned())),
        ),
        (
            "errorMessage".to_owned(),
            error_message.map_or(Value::Null, |value| Value::String(value.to_owned())),
        ),
    ]))
}

fn failed_rows(
    inputs: &[BindingRow],
    columns: &[String],
    status_alias: Option<&str>,
    status: Value,
) -> Vec<BindingRow> {
    inputs
        .iter()
        .map(|input| {
            let mut row = input.clone();
            for column in columns {
                if !row.values.contains_key(column) {
                    row.insert(column.clone(), BindingValue::Null);
                }
            }
            if let Some(alias) = status_alias {
                row.insert(alias.to_owned(), BindingValue::Scalar(status.clone()));
            }
            row
        })
        .collect()
}

fn add_mutation_counters(target: &mut QueryCounters, counters: &MutationCounters) {
    target.nodes_created = target.nodes_created.saturating_add(counters.nodes_created);
    target.nodes_deleted = target.nodes_deleted.saturating_add(counters.nodes_deleted);
    target.relationships_created = target
        .relationships_created
        .saturating_add(counters.relationships_created);
    target.relationships_deleted = target
        .relationships_deleted
        .saturating_add(counters.relationships_deleted);
    target.properties_set = target
        .properties_set
        .saturating_add(counters.properties_set);
    target.properties_removed = target
        .properties_removed
        .saturating_add(counters.properties_removed);
    target.labels_added = target.labels_added.saturating_add(counters.labels_added);
    target.labels_removed = target
        .labels_removed
        .saturating_add(counters.labels_removed);
}

fn execute_streaming_load_csv(
    runtime: &mut TransactionRuntime<'_>,
    single: &AstNode,
    csv_index: usize,
    clause: &AstNode,
    input: RowSet,
) -> QueryResult<RowSet> {
    let transaction_index = find_next_transaction_call(single, csv_index + 1)
        .ok_or_else(|| QueryError::internal("streaming LOAD CSV has no transaction CALL"))?;
    let transaction_clause = &single.children[transaction_index];
    let subquery = transaction_subquery(transaction_clause)
        .ok_or_else(|| QueryError::internal("streaming transaction CALL is missing subquery"))?;
    let pipeline = CsvTransactionPipeline {
        single,
        csv_index,
        transaction_index,
        transaction_clause,
        spec: transaction_spec(subquery)?,
        csv: compile_load_csv(clause)?,
    };
    stream_csv_batches(runtime, &pipeline, input)
}

fn stream_csv_batches(
    runtime: &mut TransactionRuntime<'_>,
    pipeline: &CsvTransactionPipeline<'_>,
    input: RowSet,
) -> QueryResult<RowSet> {
    let mut state = initial_csv_batch_state(runtime, pipeline, input.columns)?;
    if csv_prefix_requires_global_input(pipeline) {
        materialize_global_csv_prefix(runtime, pipeline, &input.rows, &mut state)?;
    } else {
        for outer in input.rows {
            stream_csv_outer(runtime, pipeline, &outer, &mut state)?;
        }
    }
    finish_pending_csv_batch(runtime, pipeline, &mut state)?;
    execute_single_from(
        runtime,
        pipeline.single,
        pipeline.transaction_index + 1,
        RowSet {
            columns: state.transaction_columns,
            rows: state.transaction_rows,
        },
    )
}

fn initial_csv_batch_state(
    runtime: &TransactionRuntime<'_>,
    pipeline: &CsvTransactionPipeline<'_>,
    mut pending_columns: Vec<String>,
) -> QueryResult<CsvBatchState> {
    if !pending_columns.contains(&pipeline.csv.binding) {
        pending_columns.push(pipeline.csv.binding.clone());
    }
    let transaction_columns = call_output_columns(
        pipeline.transaction_clause,
        &pending_columns,
        &runtime.program.source,
    )?;
    Ok(CsvBatchState {
        pending_rows: Vec::new(),
        pending_columns,
        transaction_rows: Vec::new(),
        transaction_columns,
        source_guards: Vec::new(),
        broken: false,
    })
}

fn stream_csv_outer(
    runtime: &mut TransactionRuntime<'_>,
    pipeline: &CsvTransactionPipeline<'_>,
    outer: &BindingRow,
    state: &mut CsvBatchState,
) -> QueryResult<()> {
    let (source, delimiter) = evaluate_csv_source(&pipeline.csv, |expression| {
        evaluate_at_commit(runtime, expression, outer)
    })?;
    let is_interrupted = runtime.is_interrupted;
    let guard = stream_csv_binding_rows(
        &pipeline.csv,
        outer,
        &source,
        delimiter,
        is_interrupted,
        |row| {
            let prefix = execute_read_range(
                runtime,
                pipeline.single,
                pipeline.csv_index + 1,
                pipeline.transaction_index,
                RowSet {
                    columns: row.order.clone(),
                    rows: vec![row],
                },
            )?;
            append_csv_prefix(runtime, pipeline, state, prefix)
        },
    )?;
    state.source_guards.extend(guard);
    Ok(())
}

fn csv_prefix_requires_global_input(pipeline: &CsvTransactionPipeline<'_>) -> bool {
    pipeline.single.children[pipeline.csv_index + 1..pipeline.transaction_index]
        .iter()
        .any(|clause| {
            matches!(
                clause.kind,
                AstKind::Clause(ClauseKind::With | ClauseKind::Return)
            ) && projection_requires_global_input(clause)
        })
}

fn materialize_global_csv_prefix(
    runtime: &mut TransactionRuntime<'_>,
    pipeline: &CsvTransactionPipeline<'_>,
    outers: &[BindingRow],
    state: &mut CsvBatchState,
) -> QueryResult<()> {
    let spill_connection = open_spill_connection()?;
    let mut spill = BindingSpill::create(&spill_connection)?;
    for outer in outers {
        spill_global_csv_outer(
            runtime,
            pipeline,
            outer,
            &spill_connection,
            &mut spill,
            state,
        )?;
    }
    let rows = spill.collect(&spill_connection)?;
    spill.abort(&spill_connection)?;
    let prefix = execute_read_range(
        runtime,
        pipeline.single,
        pipeline.csv_index + 1,
        pipeline.transaction_index,
        RowSet {
            columns: state.pending_columns.clone(),
            rows,
        },
    )?;
    append_csv_prefix(runtime, pipeline, state, prefix)
}

fn spill_global_csv_outer(
    runtime: &mut TransactionRuntime<'_>,
    pipeline: &CsvTransactionPipeline<'_>,
    outer: &BindingRow,
    spill_connection: &Connection,
    spill: &mut BindingSpill,
    state: &mut CsvBatchState,
) -> QueryResult<()> {
    let (source, delimiter) = evaluate_csv_source(&pipeline.csv, |expression| {
        evaluate_at_commit(runtime, expression, outer)
    })?;
    let guard = stream_csv_binding_rows(
        &pipeline.csv,
        outer,
        &source,
        delimiter,
        runtime.is_interrupted,
        |row| spill.push(spill_connection, &row),
    )?;
    state.source_guards.extend(guard);
    Ok(())
}

fn append_csv_prefix(
    runtime: &mut TransactionRuntime<'_>,
    pipeline: &CsvTransactionPipeline<'_>,
    state: &mut CsvBatchState,
    prefix: RowSet,
) -> QueryResult<()> {
    state.pending_columns = prefix.columns;
    if state.broken {
        state.transaction_rows.extend(failed_rows(
            &prefix.rows,
            &state.transaction_columns,
            pipeline.spec.status_alias.as_deref(),
            transaction_status(false, false, None, None),
        ));
        return Ok(());
    }
    state.pending_rows.extend(prefix.rows);
    state.broken = flush_complete_csv_batches(
        runtime,
        pipeline.transaction_clause,
        &pipeline.spec,
        &state.pending_columns,
        &mut state.pending_rows,
        &mut state.transaction_columns,
        &mut state.transaction_rows,
    )?;
    Ok(())
}

fn finish_pending_csv_batch(
    runtime: &mut TransactionRuntime<'_>,
    pipeline: &CsvTransactionPipeline<'_>,
    state: &mut CsvBatchState,
) -> QueryResult<()> {
    if state.broken || state.pending_rows.is_empty() {
        return Ok(());
    }
    let result = execute_transaction_call_controlled(
        runtime,
        pipeline.transaction_clause,
        RowSet {
            columns: std::mem::take(&mut state.pending_columns),
            rows: std::mem::take(&mut state.pending_rows),
        },
    )?;
    state.transaction_columns = result.rows.columns;
    state.transaction_rows.extend(result.rows.rows);
    Ok(())
}

fn execute_read_range(
    runtime: &mut TransactionRuntime<'_>,
    single: &AstNode,
    start: usize,
    end: usize,
    mut rows: RowSet,
) -> QueryResult<RowSet> {
    for clause in &single.children[start..end] {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        if kind == ClauseKind::LoadCsv
            || is_outer_write_clause(kind)
            || (kind == ClauseKind::Call && transaction_subquery(clause).is_some())
        {
            return Err(QueryError::semantic(
                "LOAD CSV transaction pipeline contains an unsupported boundary before IN TRANSACTIONS",
            ));
        }
        rows = execute_read_clause_at_commit(runtime, clause, rows)?;
    }
    Ok(rows)
}

#[allow(
    clippy::too_many_arguments,
    reason = "streaming batching mutates both pending and completed row buffers at the transaction boundary"
)]
fn flush_complete_csv_batches(
    runtime: &mut TransactionRuntime<'_>,
    transaction_clause: &AstNode,
    spec: &TransactionSpec,
    pending_columns: &[String],
    pending_rows: &mut Vec<BindingRow>,
    transaction_columns: &mut Vec<String>,
    transaction_rows: &mut Vec<BindingRow>,
) -> QueryResult<bool> {
    while let Some(first) = pending_rows.first() {
        let batch_size = resolve_batch_size(runtime, spec, first)?;
        if pending_rows.len() < batch_size {
            break;
        }
        let batch_rows = pending_rows.drain(..batch_size).collect::<Vec<_>>();
        let result = execute_transaction_call_controlled(
            runtime,
            transaction_clause,
            RowSet {
                columns: pending_columns.to_vec(),
                rows: batch_rows,
            },
        )?;
        *transaction_columns = result.rows.columns;
        transaction_rows.extend(result.rows.rows);
        if result.broke {
            transaction_rows.extend(failed_rows(
                pending_rows,
                transaction_columns,
                spec.status_alias.as_deref(),
                transaction_status(false, false, None, None),
            ));
            pending_rows.clear();
            return Ok(true);
        }
    }
    Ok(false)
}
