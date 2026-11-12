//! Feed a streamable read prefix into the existing TEMP-backed write executor.
//! The clause barrier is preserved: every prefix row is captured before any
//! mutation, rather than keeping the full prefix as an eager RowSet.

use super::*;
use crate::query::stream::program::build_program_prefix_stream;

pub(super) fn try_execute(
    base: &Snapshot<'_>,
    program: &PreparedProgram,
    params: &BTreeMap<String, Value>,
    metrics: &mut QueryMetrics,
    resources: &QueryResources,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Option<WriteOutcome>> {
    let Some((prefix, _)) = segments(program)? else {
        return Ok(None);
    };
    let Some(prepared) = program.streamed_write.as_ref() else {
        return Ok(None);
    };
    let options = program
        .write_options
        .as_ref()
        .ok_or_else(|| QueryError::internal("mutating program is missing write options"))?;
    let mut context = MutationContext::from_base(
        base.clone(),
        params,
        &options.graph_view,
        &options.access,
        resources,
        is_interrupted,
    )?;
    let snapshot = context.staged_snapshot()?;
    let graph_view = context.graph_view()?;
    let Some(stream) = build_program_prefix_stream(
        base.connection_for_query(),
        program,
        &snapshot,
        params,
        prefix,
        &context.control,
    )?
    else {
        return Ok(None);
    };
    let spill_connection = open_spill_connection()?;
    let mut rows = capture_prefix(
        stream,
        &context,
        &snapshot,
        &graph_view,
        metrics,
        &spill_connection,
    )?;
    for clause in &prepared.clauses {
        rows = execute_clause_spilled(
            &mut context,
            clause,
            &spill_connection,
            rows,
            metrics,
            is_interrupted,
        )?;
    }
    finish_spilled_write(context, prepared, spill_connection, rows, is_interrupted).map(Some)
}

pub(crate) fn prepare_streamed_write(
    snapshot: &Snapshot<'_>,
    program: &PreparedProgram,
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(PreparedWrite, MemoryReservation)>> {
    let Some((prefix, suffix)) = segments(program)? else {
        return Ok(None);
    };
    // Validate the same existing source builder before declaring any costed
    // MATCH in EXPLAIN. No prefix row or mutation executes during preparation.
    if build_program_prefix_stream(
        snapshot.connection_for_query(),
        program,
        snapshot,
        params,
        prefix,
        control,
    )?
    .is_none()
    {
        return Ok(None);
    }
    prepare_suffix(snapshot, suffix, program, params, control)
}

fn segments(program: &PreparedProgram) -> QueryResult<Option<(&[AstNode], &[AstNode])>> {
    if program.source.len() > 256 * 1024 {
        return Ok(None);
    }
    let query = executable_query(&program.root, true)?;
    if query.kind != AstKind::ComposedQuery {
        return Ok(None);
    }
    let parts = composed_query_parts(query)?;
    if !parts.rest.is_empty() || parts.first.kind != AstKind::SingleQuery {
        return Ok(None);
    }
    let clauses = &parts.first.children;
    let Some(first_write) = clauses.iter().position(|clause| {
        matches!(
            clause.kind,
            AstKind::Clause(
                ClauseKind::Create
                    | ClauseKind::Insert
                    | ClauseKind::Merge
                    | ClauseKind::Set
                    | ClauseKind::Remove
                    | ClauseKind::Delete
                    | ClauseKind::DetachDelete
            )
        )
    }) else {
        return Ok(None);
    };
    let (prefix, suffix) = clauses.split_at(first_write);
    if prefix.len() > 128
        || prefix.is_empty()
        || suffix.len() > 128
        || !prefix.iter().all(prefix_clause)
        || !suffix.iter().all(suffix_clause)
    {
        return Ok(None);
    }
    Ok(Some((prefix, suffix)))
}

fn prefix_clause(clause: &AstNode) -> bool {
    matches!(
        clause.kind,
        AstKind::Clause(
            ClauseKind::With
                | ClauseKind::Match
                | ClauseKind::OptionalMatch
                | ClauseKind::Unwind
                | ClauseKind::Let
                | ClauseKind::Filter
        )
    ) && safe_scalar_clause(clause)
}

fn suffix_clause(clause: &AstNode) -> bool {
    matches!(
        clause.kind,
        AstKind::Clause(
            ClauseKind::Create
                | ClauseKind::Insert
                | ClauseKind::Merge
                | ClauseKind::Set
                | ClauseKind::Remove
                | ClauseKind::Delete
                | ClauseKind::DetachDelete
                | ClauseKind::Match
                | ClauseKind::OptionalMatch
                | ClauseKind::Return
                | ClauseKind::Finish
        )
    ) && safe_scalar_clause(clause)
}

fn safe_scalar_clause(clause: &AstNode) -> bool {
    !crate::query::completeness::has_graph_expression(clause, false)
        && !crate::query::completeness::projection_has_aggregate(clause)
        && !clause.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::StarProjection
                    | AstKind::Search
                    | AstKind::MatchMode(_)
                    | AstKind::PathMode(_)
                    | AstKind::PathSelector(_)
                    | AstKind::Quantifier(_)
                    | AstKind::VariableLength
            )
        })
}

fn prepare_suffix(
    snapshot: &Snapshot<'_>,
    suffix: &[AstNode],
    program: &PreparedProgram,
    params: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<(PreparedWrite, MemoryReservation)>> {
    let single = AstNode {
        kind: AstKind::SingleQuery,
        span: program.root.span,
        text: None,
        children: suffix.to_vec(),
    };
    match super::super::super::prepare_write(
        snapshot,
        &single,
        &program.source,
        params,
        &program.options,
        control,
    ) {
        Ok(prepared) => Ok(Some(prepared)),
        Err(error) if error.kind == QueryErrorKind::Semantic => Ok(None),
        Err(error) => Err(error),
    }
}

fn capture_prefix(
    mut stream: crate::query::stream::program::ProgramStream,
    context: &MutationContext<'_, '_>,
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    metrics: &mut QueryMetrics,
    spill_connection: &Connection,
) -> QueryResult<BindingSpill> {
    let mut rows =
        BindingSpill::create_with_resources(spill_connection, context.control.resources().clone())?;
    while let Some(row) = stream.next_binding(
        context.connection,
        snapshot,
        graph_view,
        context.params,
        metrics,
        &context.control,
    )? {
        rows.push(spill_connection, &row, &context.control)?;
    }
    Ok(rows)
}
