use super::*;

pub(crate) fn build_program_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    mode: ExecutionMode,
    params: &std::collections::BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<ProgramStream>> {
    if mode == ExecutionMode::Profile
        || !program.public_result
        || program.writes
        || program.version_operation
        || program.transaction_options.is_some()
        || has_nonstreamable_operator(program)
    {
        return Ok(None);
    }

    let query = crate::query::completeness::executable_query(&program.root, true)?;
    let stream = build_query_stream(
        &ClauseStreamBuilder {
            connection,
            program,
            snapshot,
            params,
            control,
        },
        query,
    )?;
    let Some(stream) = stream else {
        return Ok(None);
    };
    if stream.columns != program.columns {
        return Ok(None);
    }
    Ok(Some(stream))
}

fn build_query_stream(
    builder: &ClauseStreamBuilder<'_, '_>,
    query: &AstNode,
) -> QueryResult<Option<ProgramStream>> {
    let query = unbrace_query(query)?;
    match query.kind {
        AstKind::SingleQuery => build_clause_stream(
            builder,
            Box::new(SeedSource { emitted: false }),
            Vec::new(),
            &query.children,
        ),
        AstKind::ComposedQuery => build_composed_union_stream(builder, query),
        _ => Ok(None),
    }
}

fn build_query_stream_from_input(
    builder: &ClauseStreamBuilder<'_, '_>,
    query: &AstNode,
    source: Box<dyn BindingSource>,
    columns: Vec<String>,
) -> QueryResult<Option<ProgramStream>> {
    let query = unbrace_query(query)?;
    match query.kind {
        AstKind::SingleQuery => build_clause_stream(builder, source, columns, &query.children),
        _ => Ok(None),
    }
}

fn unbrace_query(mut query: &AstNode) -> QueryResult<&AstNode> {
    while query.kind == AstKind::Subquery(SubqueryKind::Braced) {
        let body = crate::query::completeness::required_query_body(
            query,
            "braced query is missing its body",
        )?;
        query = crate::query::completeness::executable_query(body, true)?;
    }
    Ok(query)
}

fn build_composed_union_stream(
    builder: &ClauseStreamBuilder<'_, '_>,
    query: &AstNode,
) -> QueryResult<Option<ProgramStream>> {
    let parts = crate::query::completeness::composed_query_parts(query)?;
    let has_next = parts
        .rest
        .iter()
        .any(|(connector, _)| *connector == QueryConnector::Next);
    if has_next {
        if parts
            .rest
            .iter()
            .any(|(connector, _)| *connector != QueryConnector::Next)
        {
            return Ok(None);
        }
        let Some(mut current) = build_query_stream(builder, parts.first)? else {
            return Ok(None);
        };
        let mut previous = parts.first;
        for (_, operand) in parts.rest {
            let ProgramStream {
                source, columns, ..
            } = current;
            let (source, columns) = if crate::cypher::query_body_returns_columns(previous) {
                (source, columns)
            } else if crate::cypher::query_body_ends_with_call(previous) {
                (
                    Box::new(UnitMapSource { upstream: source }) as Box<dyn BindingSource>,
                    Vec::new(),
                )
            } else {
                return Ok(None);
            };
            let Some(next) = build_query_stream_from_input(builder, operand, source, columns)?
            else {
                return Ok(None);
            };
            current = next;
            previous = operand;
        }
        return Ok(Some(current));
    }
    let Some(first) = build_query_stream(builder, parts.first)? else {
        return Ok(None);
    };
    let ProgramStream {
        mut source,
        columns,
        ..
    } = first;
    for (connector, operand) in parts.rest {
        let Some(right) = build_query_stream(builder, operand)? else {
            return Ok(None);
        };
        if right.columns != columns {
            return Err(QueryError::internal(format!(
                "UNION stream columns {:?} differ from {:?}",
                right.columns, columns
            )));
        }
        source = Box::new(ConcatSource {
            left: source,
            right: right.source,
            left_done: false,
        });
        if connector != QueryConnector::UnionAll {
            source = Box::new(DistinctBindingSource {
                upstream: source,
                columns: columns.clone(),
                state: DistinctBindingState::Pending,
            });
        }
    }
    Ok(Some(ProgramStream {
        source,
        columns,
        output_memory: None,
    }))
}

pub(crate) fn build_program_prefix_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    clauses: &[AstNode],
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<ProgramStream>> {
    build_clause_stream(
        &ClauseStreamBuilder {
            connection,
            program,
            snapshot,
            params,
            control,
        },
        Box::new(SeedSource { emitted: false }),
        Vec::new(),
        clauses,
    )
}

pub(crate) fn build_program_suffix_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    mut input: crate::query::completeness::execute::RowSet,
    clauses: &[AstNode],
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<ProgramStream>> {
    input.track(control)?;
    let mut memory = input
        .memory
        .take()
        .ok_or_else(|| QueryError::internal("program suffix input lost its memory reservation"))?;
    memory.grow(crate::query::resources::allocation_bytes::<BindingRow>(
        input.rows.capacity(),
    )?)?;
    build_clause_stream(
        &ClauseStreamBuilder {
            connection,
            program,
            snapshot,
            params,
            control,
        },
        Box::new(RowsSource {
            rows: input.rows.into(),
            memory,
        }),
        input.columns,
        clauses,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the spilled suffix builder keeps execution state explicit at one internal boundary"
)]
pub(crate) fn build_program_spilled_suffix_stream(
    connection: &Connection,
    program: &PreparedProgram,
    snapshot: &Snapshot<'_>,
    params: &std::collections::BTreeMap<String, Value>,
    input_columns: Vec<String>,
    spill_connection: Connection,
    spill: BindingSpill,
    clauses: &[AstNode],
    control: &ExecutionControl<'_>,
) -> QueryResult<Option<ProgramStream>> {
    build_clause_stream(
        &ClauseStreamBuilder {
            connection,
            program,
            snapshot,
            params,
            control,
        },
        Box::new(SpilledSource {
            connection: spill_connection,
            spill,
            after: -1,
            buffered: std::collections::VecDeque::new(),
            memory: None,
            done: false,
        }),
        input_columns,
        clauses,
    )
}

fn build_clause_stream(
    builder: &ClauseStreamBuilder<'_, '_>,
    mut source: Box<dyn BindingSource>,
    mut columns: Vec<String>,
    clauses: &[AstNode],
) -> QueryResult<Option<ProgramStream>> {
    for clause in clauses {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        let next_columns = crate::query::completeness::infer_clause_columns(
            clause,
            &columns,
            &builder.program.source,
        )?;
        source = match builder.apply(source, &columns, &next_columns, clause, kind)? {
            Some(source) => source,
            None => return Ok(None),
        };
        columns = next_columns;
    }
    Ok(Some(ProgramStream {
        source,
        columns,
        output_memory: None,
    }))
}

struct ClauseStreamBuilder<'a, 'snapshot> {
    connection: &'a Connection,
    program: &'a PreparedProgram,
    snapshot: &'a Snapshot<'snapshot>,
    params: &'a std::collections::BTreeMap<String, Value>,
    control: &'a ExecutionControl<'a>,
}

impl ClauseStreamBuilder<'_, '_> {
    fn apply(
        &self,
        source: Box<dyn BindingSource>,
        columns: &[String],
        next_columns: &[String],
        clause: &AstNode,
        kind: ClauseKind,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        if matches!(
            kind,
            ClauseKind::Filter | ClauseKind::Let | ClauseKind::Unwind | ClauseKind::For
        ) && clause_requires_graph_executor(clause)
        {
            return Ok(Some(self.batch_source(
                source,
                clause,
                columns,
                next_columns,
            )));
        }
        match kind {
            ClauseKind::Match | ClauseKind::OptionalMatch => {
                self.match_source(source, columns, next_columns, clause, kind)
            }
            ClauseKind::Filter => self.filter_source(source, clause),
            ClauseKind::Let => self.let_source(source, clause),
            ClauseKind::Unwind | ClauseKind::For => self.unwind_source(source, clause),
            ClauseKind::LoadCsv => Ok(Some(Box::new(LoadCsvSource {
                upstream: source,
                spec: compile_load_csv(clause)?,
                current_input: None,
                current_stream: None,
                input_memory: None,
            }))),
            ClauseKind::Call if call_clause_can_batch_stream(clause) => Ok(Some(
                self.batch_source(source, clause, columns, next_columns),
            )),
            ClauseKind::Show => Ok(Some(self.batch_source(
                source,
                clause,
                columns,
                next_columns,
            ))),
            ClauseKind::With | ClauseKind::Return => {
                self.projection_source(source, columns, next_columns, clause, kind)
            }
            _ => Ok(None),
        }
    }

    fn match_source(
        &self,
        source: Box<dyn BindingSource>,
        columns: &[String],
        next_columns: &[String],
        clause: &AstNode,
        kind: ClauseKind,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        if match_requires_complex_executor(clause) {
            return Ok(Some(Box::new(BatchClauseSource {
                upstream: source,
                clause: clause.clone(),
                source: self.program.source.clone(),
                input_columns: columns.to_vec(),
                output_columns: next_columns.to_vec(),
                buffered: std::collections::VecDeque::new(),
                done: false,
                buffered_memory: None,
                input_batch_rows: 1,
            })));
        }
        let Ok(mut step) = crate::query::plan::lower_match(
            self.connection,
            clause,
            kind == ClauseKind::OptionalMatch,
        ) else {
            return Ok(None);
        };
        let index_memory = if let Ok(schema) = self.snapshot.schema_state() {
            crate::query::schema::select_standard_index_seeks(
                &schema,
                std::slice::from_mut(&mut step),
                self.params,
                &self.program.options.access.read.query,
                self.snapshot,
                self.control,
            )?
        } else {
            self.control.reserve(0)?
        };
        Ok(Some(Box::new(MatchSource {
            _index_memory: index_memory,
            upstream: source,
            step,
            current: None,
        })))
    }

    fn filter_source(
        &self,
        source: Box<dyn BindingSource>,
        clause: &AstNode,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        Ok(Some(Box::new(FilterSource {
            upstream: source,
            predicate: first_expression(clause, "FILTER is missing its predicate")?,
        })))
    }

    fn let_source(
        &self,
        source: Box<dyn BindingSource>,
        clause: &AstNode,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        Ok(Some(Box::new(LetSource {
            upstream: source,
            bindings: expression::compile_let_bindings(clause)?,
        })))
    }

    fn unwind_source(
        &self,
        source: Box<dyn BindingSource>,
        clause: &AstNode,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        let (name, expression) = crate::query::completeness::execute::compile_unwind(clause)?;
        Ok(Some(Box::new(UnwindSource {
            upstream: source,
            name,
            expression,
            current_input: None,
            current_values: Vec::new().into_iter(),
            values_memory: None,
            input_memory: None,
        })))
    }

    fn projection_source(
        &self,
        source: Box<dyn BindingSource>,
        columns: &[String],
        next_columns: &[String],
        clause: &AstNode,
        kind: ClauseKind,
    ) -> QueryResult<Option<Box<dyn BindingSource>>> {
        if projection_requires_global_executor(clause) {
            return Ok(None);
        }
        if kind == ClauseKind::With && clause.descendants().any(|node| node.kind == AstKind::Where)
        {
            if projection_has_global_modifier(clause) {
                return Ok(None);
            }
            return Ok(Some(self.batch_source(
                source,
                clause,
                columns,
                next_columns,
            )));
        }
        if projection_requires_batch_executor(clause) {
            return Ok(Some(self.batch_source(
                source,
                clause,
                columns,
                next_columns,
            )));
        }
        let Ok(plan) =
            crate::query::plan::lower_projection(clause, &self.program.source, self.params)
        else {
            return Ok(None);
        };
        let mut source = self.lower_projection_source(source, next_columns, plan);
        if kind == ClauseKind::With
            && let Some(where_node) = clause
                .descendants()
                .find(|node| node.kind == AstKind::Where)
        {
            source = Box::new(FilterSource {
                upstream: source,
                predicate: first_expression(where_node, "WITH WHERE is missing its predicate")?,
            });
        }
        Ok(Some(source))
    }

    fn lower_projection_source(
        &self,
        source: Box<dyn BindingSource>,
        next_columns: &[String],
        plan: ProjectionPlan,
    ) -> Box<dyn BindingSource> {
        let ProjectionPlan {
            projections,
            order,
            skip,
            limit,
            distinct,
        } = plan;
        if order.is_empty() && !distinct {
            return Box::new(ProjectionSource {
                upstream: source,
                projections,
                skip,
                limit,
                seen: 0,
                emitted: 0,
            });
        }
        Box::new(BarrierProjectionSource {
            upstream: source,
            projections,
            order,
            distinct,
            skip,
            limit,
            columns: next_columns.to_vec(),
            state: BarrierProjectionState::Pending,
        })
    }

    fn batch_source(
        &self,
        source: Box<dyn BindingSource>,
        clause: &AstNode,
        columns: &[String],
        next_columns: &[String],
    ) -> Box<dyn BindingSource> {
        batch_clause_source(source, clause, self.program, columns, next_columns)
    }
}

fn has_nonstreamable_operator(program: &PreparedProgram) -> bool {
    program.logical.operators.iter().any(|operator| {
        matches!(
            operator,
            LogicalOperator::Aggregate
                | LogicalOperator::When
                | LogicalOperator::Eager
                | LogicalOperator::Mutation { .. }
                | LogicalOperator::Schema { .. }
                | LogicalOperator::Commit
        )
    })
}

fn match_requires_complex_executor(clause: &AstNode) -> bool {
    clause_requires_graph_executor(clause)
        || clause.descendants().any(|node| {
            matches!(
                node.kind,
                AstKind::Search
                    | AstKind::MatchMode(_)
                    | AstKind::PathMode(_)
                    | AstKind::PathSelector(_)
                    | AstKind::Quantifier(_)
                    | AstKind::VariableLength
            )
        })
}

fn projection_requires_global_executor(clause: &AstNode) -> bool {
    crate::query::completeness::projection_has_aggregate(clause)
        || interpolation_contains(
            clause,
            |expression| matches!(expression, Expr::Function { name, .. } if crate::query::registry::is_aggregating(name)),
        )
        || (projection_requires_batch_executor(clause) && projection_has_global_modifier(clause))
}

fn projection_requires_batch_executor(clause: &AstNode) -> bool {
    clause_requires_graph_executor(clause)
        || clause
            .descendants()
            .any(|node| node.kind == AstKind::StarProjection)
}

fn projection_has_global_modifier(clause: &AstNode) -> bool {
    clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::OrderBy
                | AstKind::Skip
                | AstKind::Limit
                | AstKind::SetQuantifier(crate::cypher::SetQuantifierKind::Distinct)
        )
    })
}

fn clause_contains_subquery(clause: &AstNode) -> bool {
    clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::Subquery(_) | AstKind::Expression(ExpressionKind::Subquery)
        )
    })
}

fn clause_requires_graph_executor(clause: &AstNode) -> bool {
    clause_contains_subquery(clause)
        || crate::query::completeness::has_graph_expression(clause, false)
        || interpolation_contains(clause, |expression| {
            matches!(
                expression,
                Expr::Subquery { .. } | Expr::PatternPredicate(_) | Expr::PatternComprehension(_)
            )
        })
}

fn interpolation_contains(clause: &AstNode, predicate: impl Fn(&Expr) -> bool) -> bool {
    clause
        .descendants()
        .filter(|node| node.kind == AstKind::Expression(ExpressionKind::InterpolatedString))
        .any(|node| {
            // Interpolation is atomic in the source AST. Inspect its compiled
            // expressions once while selecting the stream operator, rather
            // than sending graph subqueries to the scalar evaluator.
            let Ok(expression) = expression::compile_expression(node) else {
                return true;
            };
            let mut found = false;
            let _ = expression::transform_expression(&expression, &mut |candidate| {
                found |= predicate(candidate);
                Ok(None)
            });
            found
        })
}

fn batch_clause_source(
    upstream: Box<dyn BindingSource>,
    clause: &AstNode,
    program: &PreparedProgram,
    input_columns: &[String],
    output_columns: &[String],
) -> Box<dyn BindingSource> {
    Box::new(BatchClauseSource {
        upstream,
        clause: clause.clone(),
        source: program.source.clone(),
        input_columns: input_columns.to_vec(),
        output_columns: output_columns.to_vec(),
        buffered: std::collections::VecDeque::new(),
        done: false,
        buffered_memory: None,
        input_batch_rows: 1,
    })
}

fn call_clause_can_batch_stream(clause: &AstNode) -> bool {
    if clause_contains_subquery(clause) {
        return true;
    }
    let Some(name) = clause
        .descendants()
        .find(|node| node.kind == AstKind::FunctionName)
        .and_then(|node| node.text.as_deref())
    else {
        return false;
    };
    !crate::query::registry::is_version_procedure(name)
        && !crate::query::registry::is_semantic_maintenance(name)
}

fn first_expression(node: &AstNode, message: &str) -> QueryResult<Expr> {
    let expression = expression::surface_expressions(node)
        .into_iter()
        .next()
        .ok_or_else(|| QueryError::semantic(message))?;
    expression::compile_expression(expression)
}
