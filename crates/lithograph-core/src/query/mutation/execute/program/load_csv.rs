use crate::query::completeness::projection_requires_global_input;
use crate::query::ingestion::{
    CompiledLoadCsv, compile_load_csv, evaluate_csv_source, stream_csv_binding_rows,
};
use crate::query::spill::{
    BINDING_SPILL_BATCH_ROWS, BindingSpill, SpilledBindings, open_spill_connection,
};

use super::super::*;
use super::*;

#[allow(
    clippy::too_many_arguments,
    reason = "the LOAD CSV clause pipeline keeps mutation state and cancellation explicit"
)]
pub(super) fn execute_load_csv_tail(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    single: &AstNode,
    start: usize,
    input: RowSet,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<RowSet> {
    let spill_connection = open_spill_connection()?;
    let mut current = SpilledBindings::from_rows(&spill_connection, input.columns, &input.rows)?;
    let mut csv_guards = Vec::new();
    for clause in single.children.iter().skip(start) {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        check_interrupted(is_interrupted)?;
        if kind == ClauseKind::Finish {
            current.spill.abort(&spill_connection)?;
            return Ok(RowSet {
                columns: Vec::new(),
                rows: Vec::new(),
            });
        }
        current = execute_spilled_tail_clause(
            context,
            program,
            clause,
            kind,
            current,
            &spill_connection,
            &mut csv_guards,
            metrics,
            is_interrupted,
        )?;
    }
    let (columns, rows) = current.into_rows(&spill_connection)?;
    Ok(RowSet { columns, rows })
}

#[allow(
    clippy::too_many_arguments,
    reason = "clause dispatch preserves the existing mutation context without another execution abstraction"
)]
fn execute_spilled_tail_clause(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    kind: ClauseKind,
    current: SpilledBindings,
    spill_connection: &Connection,
    csv_guards: &mut Vec<tempfile::TempPath>,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpilledBindings> {
    match kind {
        ClauseKind::LoadCsv => expand_load_csv(
            context,
            program,
            clause,
            current,
            spill_connection,
            csv_guards,
            is_interrupted,
        ),
        ClauseKind::Create | ClauseKind::Insert => execute_create_spill(
            context,
            program,
            clause,
            current,
            spill_connection,
            is_interrupted,
        ),
        ClauseKind::Set => execute_set_spill(
            context,
            program,
            clause,
            current,
            spill_connection,
            is_interrupted,
        ),
        ClauseKind::Remove => execute_remove_spill(
            context,
            program,
            clause,
            current,
            spill_connection,
            is_interrupted,
        ),
        ClauseKind::Delete | ClauseKind::DetachDelete => execute_delete_spill(
            context,
            clause,
            kind == ClauseKind::DetachDelete,
            current,
            spill_connection,
            is_interrupted,
        ),
        ClauseKind::Merge => execute_merge_spill(
            context,
            program,
            clause,
            current,
            spill_connection,
            is_interrupted,
        ),
        _ => execute_other_spilled_clause(
            context,
            program,
            clause,
            kind,
            current,
            spill_connection,
            metrics,
            is_interrupted,
        ),
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "CSV expansion keeps the staged Snapshot and source lifetime explicit"
)]
fn expand_load_csv(
    context: &MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    current: SpilledBindings,
    spill_connection: &Connection,
    csv_guards: &mut Vec<tempfile::TempPath>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpilledBindings> {
    let spec = compile_load_csv(clause)?;
    let snapshot = context.staged_snapshot()?;
    let columns = crate::query::completeness::infer_clause_columns(
        clause,
        &current.columns,
        &program.source,
    )?;
    current.transform_batches(spill_connection, columns, |_, rows, output| {
        for input_row in rows {
            expand_load_csv_input(
                context,
                &snapshot,
                &spec,
                input_row,
                spill_connection,
                output,
                csv_guards,
                is_interrupted,
            )?;
        }
        Ok(())
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "one CSV source expansion keeps evaluation, spill output, source lifetime and cancellation explicit"
)]
fn expand_load_csv_input(
    context: &MutationContext<'_, '_>,
    snapshot: &Snapshot<'_>,
    spec: &CompiledLoadCsv,
    input_row: BindingRow,
    spill_connection: &Connection,
    output: &mut BindingSpill,
    csv_guards: &mut Vec<tempfile::TempPath>,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<()> {
    let (source, delimiter) = evaluate_csv_source(spec, |value| {
        expression::evaluate(value, snapshot, &input_row, context.params)
    })?;
    let guard = stream_csv_binding_rows(
        spec,
        &input_row,
        &source,
        delimiter,
        is_interrupted,
        |row| output.push(spill_connection, &row),
    )?;
    csv_guards.extend(guard);
    Ok(())
}

fn execute_create_spill(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    current: SpilledBindings,
    spill_connection: &Connection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpilledBindings> {
    let pattern = lower_write_pattern(clause)?;
    execute_stateful_write_spill(
        context,
        program,
        clause,
        current,
        spill_connection,
        |context, rows, state| {
            execute_create_clause_batch(context, &pattern, rows, state, is_interrupted)
        },
    )
}

fn execute_set_spill(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    current: SpilledBindings,
    spill_connection: &Connection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpilledBindings> {
    let items = lower_set_items(clause)?;
    execute_stateful_write_spill(
        context,
        program,
        clause,
        current,
        spill_connection,
        |context, rows, state| {
            execute_set_clause_batch(context, &items, rows, state, is_interrupted)
        },
    )
}

fn execute_remove_spill(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    current: SpilledBindings,
    spill_connection: &Connection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpilledBindings> {
    let items = lower_remove_items(clause)?;
    execute_stateful_write_spill(
        context,
        program,
        clause,
        current,
        spill_connection,
        |context, rows, state| {
            execute_remove_clause_batch(context, &items, rows, state, is_interrupted)
        },
    )
}

fn execute_stateful_write_spill(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    current: SpilledBindings,
    spill_connection: &Connection,
    mut mutate: impl FnMut(
        &mut MutationContext<'_, '_>,
        Vec<BindingRow>,
        &mut MutationClauseState<'_>,
    ) -> QueryResult<Vec<BindingRow>>,
) -> QueryResult<SpilledBindings> {
    let mut state = begin_mutation_clause(context)?;
    let columns = crate::query::completeness::infer_clause_columns(
        clause,
        &current.columns,
        &program.source,
    )?;
    let result = current.transform_batches(spill_connection, columns, |_, rows, output| {
        let rows = mutate(context, rows, &mut state)?;
        output.push_all(spill_connection, &rows)
    })?;
    finish_mutation_clause(context, &state)?;
    Ok(result)
}

fn execute_delete_spill(
    context: &mut MutationContext<'_, '_>,
    clause: &AstNode,
    detach: bool,
    current: SpilledBindings,
    spill_connection: &Connection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpilledBindings> {
    let expressions = lower_delete_expressions(clause)?;
    let variables = delete_target_variables(&expressions);
    let state = begin_mutation_clause(context)?;
    let mut targets = BindingSpill::create(spill_connection)?;
    current
        .spill
        .for_each_batch(spill_connection, BINDING_SPILL_BATCH_ROWS, |rows| {
            let (_, rows) = evaluate_delete_targets(context, &expressions, &rows, &state)?;
            delete::validate_delete_values(&rows, &variables)?;
            targets.push_all(spill_connection, &rows)
        })?;
    targets.for_each_batch(spill_connection, BINDING_SPILL_BATCH_ROWS, |rows| {
        delete::apply_delete_relationships(
            context,
            &rows,
            &variables,
            &state.clause_input,
            &state.graph_view,
            is_interrupted,
        )
    })?;
    targets.for_each_batch(spill_connection, BINDING_SPILL_BATCH_ROWS, |rows| {
        delete::apply_delete_nodes(
            context,
            &rows,
            &variables,
            detach,
            &state.clause_input,
            &state.graph_view,
            is_interrupted,
        )
    })?;
    targets.abort(spill_connection)?;
    Ok(current)
}

fn execute_merge_spill(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    current: SpilledBindings,
    spill_connection: &Connection,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpilledBindings> {
    let merge = lower_merge(clause)?;
    execute_stateful_write_spill(
        context,
        program,
        clause,
        current,
        spill_connection,
        |context, rows, state| {
            execute_merge_clause_batch(context, &merge, rows, state, is_interrupted)
        },
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the generic row-local tail keeps the existing clause dispatcher and query context"
)]
fn execute_other_spilled_clause(
    context: &mut MutationContext<'_, '_>,
    program: &PreparedProgram,
    clause: &AstNode,
    kind: ClauseKind,
    current: SpilledBindings,
    spill_connection: &Connection,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<SpilledBindings> {
    if matches!(kind, ClauseKind::With | ClauseKind::Return)
        && projection_requires_global_input(clause)
    {
        let (columns, rows) = current.into_rows(spill_connection)?;
        let result = execute_single_clause(
            context,
            program,
            clause,
            kind,
            RowSet { columns, rows },
            metrics,
            is_interrupted,
        )?;
        return SpilledBindings::from_rows(spill_connection, result.columns, &result.rows);
    }

    let columns = crate::query::completeness::infer_clause_columns(
        clause,
        &current.columns,
        &program.source,
    )?;
    current.transform_batches(spill_connection, columns, |input_columns, rows, output| {
        let result = execute_single_clause(
            context,
            program,
            clause,
            kind,
            RowSet {
                columns: input_columns.to_vec(),
                rows,
            },
            metrics,
            is_interrupted,
        )?;
        output.push_all(spill_connection, &result.rows)
    })
}
