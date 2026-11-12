//! Prepare basic reads with one query-wide optional planning budget.

use super::*;

pub(super) fn prepare_read_query(
    connection: &Connection,
    single: &AstNode,
    query: &str,
    context: PrepareContext,
    control: &crate::query::ExecutionControl<'_>,
) -> QueryResult<PreparedQuery> {
    let (mut matches, return_clause) = lower_clauses(connection, single)?;
    for step in &mut matches {
        select_node_id_seeks(step, &context.params);
    }
    let ProjectionPlan {
        projections,
        order,
        skip,
        limit,
        distinct,
    } = lower_projection(return_clause, query, &context.params)?;
    let aggregate = validate_aggregation(&projections)?;
    let resolved_state = resolve_planning_state(connection, &context, control)?;
    let snapshot = storage::Snapshot::from_resolved_state(connection, &resolved_state);
    resolved_state.query_schema_state().ok_or_else(|| {
        QueryError::internal("read query resolved state is missing its versioned Schema")
    })?;
    let mut budget = PlanningBudget::default();
    let mut index_memory = control.reserve(0)?;
    let mut statistics = PlannerStatistics::default();
    let mut incoming = Vec::new();
    let clauses = single.children.iter().filter(|node| {
        matches!(
            node.kind,
            AstKind::Clause(ClauseKind::Match | ClauseKind::OptionalMatch)
        )
    });
    for (step, clause) in matches.iter_mut().zip(clauses) {
        let declared = crate::query::completeness::infer_clause_columns(clause, &incoming, query)?;
        let scope = join::JoinClauseScope {
            clause_start: clause.span.start,
            snapshot_identity: snapshot.cache_identity(),
            incoming: &incoming,
            declared_columns: &declared,
        };
        if constrain_match_scope(step, &incoming) {
            let (selected_stats, selected_memory) = plan_access(
                &snapshot,
                &context.graph_view,
                std::slice::from_mut(step),
                &context.params,
                control,
                &mut budget,
                Some(&scope),
            )?;
            statistics = selected_stats;
            index_memory.merge(selected_memory)?;
        }
        incoming = declared;
    }
    let columns = output_columns(context.mode, &projections);
    let (logical, profile_sites) = build_logical(
        &matches,
        !order.is_empty(),
        skip > 0,
        limit.is_some(),
        distinct,
        aggregate,
    );
    let physical = build_physical(&logical, &matches, !order.is_empty());
    let mut prepared = PreparedQuery {
        resources: crate::query::QueryResources::default(),
        metrics: crate::query::stream::QueryMetrics::default(),
        params_memory: Some(index_memory),
        access: context.access,
        commit: context.commit,
        candidate: context.candidate,
        resolved_state: Some(resolved_state),
        graph_view: context.graph_view,
        matches,
        profile_sites,
        projections,
        order,
        ordered_topk: None,
        skip,
        limit,
        distinct,
        aggregate,
        params: context.params,
        mode: context.mode,
        write: None,
        schema: None,
        program: None,
        logical,
        physical,
        statistics,
        columns,
    };
    crate::query::stream::topk::install(&mut prepared, &snapshot, single, &mut budget, control)?;
    Ok(prepared)
}
