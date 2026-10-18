use crate::query::completeness::projection_requires_global_input;
use crate::query::ingestion::{
    CompiledLoadCsv, compile_load_csv, evaluate_csv_source, stream_csv_binding_rows,
};
use crate::query::spill::{SpilledBindings, open_spill_connection};

use super::*;

pub(super) fn execute_load_csv_tail(
    executor: &mut ReadExecutor<'_, '_>,
    single: &AstNode,
    start: usize,
    input: RowSet,
) -> QueryResult<RowSet> {
    let spill_connection = open_spill_connection()?;
    let mut current = SpilledBindings::from_rows_with_resources(
        &spill_connection,
        input.columns,
        &input.rows,
        executor.control.resources().clone(),
        executor.control,
    )?;
    let mut csv_guards = Vec::new();
    for clause in single.children.iter().skip(start) {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        executor.check_interrupted()?;
        if kind == ClauseKind::Finish {
            current.spill.abort(&spill_connection)?;
            return Ok(RowSet {
                columns: Vec::new(),
                rows: Vec::new(),
                memory: None,
            });
        }
        let next = if kind == ClauseKind::LoadCsv {
            expand_load_csv(
                executor,
                clause,
                current,
                &spill_connection,
                &mut csv_guards,
            )?
        } else {
            execute_spilled_clause(executor, clause, kind, current, &spill_connection)?
        };
        current = next;
    }
    let (columns, rows, memory) =
        current.into_rows_reserved(&spill_connection, executor.control)?;
    drop(csv_guards);
    Ok(RowSet {
        columns,
        rows,
        memory: Some(memory),
    })
}

fn expand_load_csv(
    executor: &mut ReadExecutor<'_, '_>,
    clause: &AstNode,
    current: SpilledBindings,
    spill_connection: &Connection,
    csv_guards: &mut Vec<tempfile::TempPath>,
) -> QueryResult<SpilledBindings> {
    let spec = compile_load_csv(clause)?;
    let columns = crate::query::completeness::infer_clause_columns(
        clause,
        &current.columns,
        executor.source,
    )?;
    current.transform_batches(
        spill_connection,
        columns,
        executor.control,
        |_, rows, output| {
            for input_row in rows {
                expand_load_csv_input(
                    executor,
                    &spec,
                    input_row,
                    spill_connection,
                    output,
                    csv_guards,
                )?;
            }
            Ok(())
        },
    )
}

fn expand_load_csv_input(
    executor: &mut ReadExecutor<'_, '_>,
    spec: &CompiledLoadCsv,
    input_row: BindingRow,
    spill_connection: &Connection,
    output: &mut crate::query::spill::BindingSpill,
    csv_guards: &mut Vec<tempfile::TempPath>,
) -> QueryResult<()> {
    let (source, delimiter) =
        evaluate_csv_source(spec, |expression| executor.evaluate(expression, &input_row))?;
    let guard = stream_csv_binding_rows(
        spec,
        &input_row,
        &source,
        delimiter,
        executor.control,
        |row, _memory| output.push(spill_connection, &row, executor.control),
    )?;
    csv_guards.extend(guard);
    Ok(())
}

fn execute_spilled_clause(
    executor: &mut ReadExecutor<'_, '_>,
    clause: &AstNode,
    kind: ClauseKind,
    current: SpilledBindings,
    spill_connection: &Connection,
) -> QueryResult<SpilledBindings> {
    if matches!(kind, ClauseKind::With | ClauseKind::Return)
        && projection_requires_global_input(clause)
    {
        let (columns, rows, memory) =
            current.into_rows_reserved(spill_connection, executor.control)?;
        let result = executor.execute_single_clause(
            clause,
            kind,
            RowSet {
                columns,
                rows,
                memory: Some(memory),
            },
        )?;
        return SpilledBindings::from_rows_with_resources(
            spill_connection,
            result.columns,
            &result.rows,
            executor.control.resources().clone(),
            executor.control,
        );
    }

    let columns = crate::query::completeness::infer_clause_columns(
        clause,
        &current.columns,
        executor.source,
    )?;
    current.transform_batches(
        spill_connection,
        columns,
        executor.control,
        |input_columns, rows, output| {
            let result = executor.execute_single_clause(
                clause,
                kind,
                RowSet {
                    columns: input_columns.to_vec(),
                    rows,
                    memory: None,
                },
            )?;
            output.push_all(spill_connection, &result.rows, executor.control)
        },
    )
}
