use super::*;

impl ReadExecutor<'_, '_> {
    pub(super) fn materialize_subqueries(
        &mut self,
        value: &expression::Expr,
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<(expression::Expr, MemoryReservation)> {
        self.transform_runtime_expression(value, &mut |executor, candidate| {
            if let Some(value) =
                evaluate_lazy_graph_expression(candidate, executor.control, &mut |nested| {
                    executor.evaluate_owned_with_aliases(nested, row, aliases)
                })?
            {
                executor.expression_reservation.merge(value.reservation)?;
                return Ok(Some(expression::Expr::Literal(value.value)));
            }
            if let Some(value) = evaluate_scoped_graph_expression(
                candidate,
                aliases,
                executor.control,
                &mut |nested, locals| {
                    let (local, _memory) =
                        scoped_binding_row(&executor.snapshot, row, locals, executor.control)?;
                    executor.evaluate_owned_with_aliases(nested, &local, locals)
                },
            )? {
                executor.expression_reservation.merge(value.reservation)?;
                return Ok(Some(expression::Expr::Literal(value.value)));
            }
            Ok(executor
                .evaluate_graph_runtime_candidate(candidate, row, aliases)?
                .map(expression::Expr::Literal))
        })
    }

    pub(super) fn transform_runtime_expression(
        &mut self,
        value: &expression::Expr,
        transform: &mut impl FnMut(
            &mut Self,
            &expression::Expr,
        ) -> QueryResult<Option<expression::Expr>>,
    ) -> QueryResult<(expression::Expr, MemoryReservation)> {
        let mut memory = self.control.reserve(0)?;
        let expression = expression::transform_expression(value, &mut |candidate| {
            if let Some((value, reservation)) = self.copy_literal(candidate)? {
                memory.merge(reservation)?;
                return Ok(Some(value));
            }
            transform(self, candidate)
        })?;
        Ok((expression, memory))
    }

    pub(super) fn copy_literal(
        &mut self,
        candidate: &expression::Expr,
    ) -> QueryResult<Option<(expression::Expr, MemoryReservation)>> {
        let expression::Expr::Literal(value) = candidate else {
            return Ok(None);
        };
        let reservation = self.control.reserve(expression::value_bytes_with_control(
            value,
            Some(self.control),
        )?)?;
        let value = expression::clone_value_unreserved_controlled(value, Some(self.control))?;
        Ok(Some((expression::Expr::Literal(value), reservation)))
    }

    pub(super) fn evaluate_graph_runtime_candidate(
        &mut self,
        candidate: &expression::Expr,
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<Option<Value>> {
        use expression::Expr;
        if !matches!(
            candidate,
            Expr::Subquery { .. } | Expr::PatternPredicate(_) | Expr::PatternComprehension(_)
        ) {
            return Ok(None);
        }
        let local;
        let _local_memory;
        let row = if aliases.is_empty() {
            row
        } else {
            (local, _local_memory) =
                scoped_binding_row(&self.snapshot, row, aliases, self.control)?;
            &local
        };
        let value = match candidate {
            Expr::Subquery { kind, node } => self.evaluate_subquery_expression(*kind, node, row)?,
            Expr::PatternPredicate(node) => self.evaluate_pattern_predicate(node, row)?,
            Expr::PatternComprehension(node) => self.evaluate_pattern_comprehension(node, row)?,
            _ => unreachable!("graph runtime candidate was checked above"),
        };
        Ok(Some(value))
    }

    pub(super) fn evaluate_pattern_predicate(
        &mut self,
        pattern: &AstNode,
        row: &BindingRow,
    ) -> QueryResult<Value> {
        let clause = pattern_expression_clause(pattern, None)?;
        let (matches, _matches_memory) = self.execute_pattern_expression(&clause, row)?;
        Ok(Value::Boolean(!matches.is_empty()))
    }

    pub(super) fn evaluate_pattern_comprehension(
        &mut self,
        node: &AstNode,
        row: &BindingRow,
    ) -> QueryResult<Value> {
        let (clause, projection, predicate) = pattern_comprehension_plan(node)?;
        let (mut matches, _matches_memory) = self.execute_pattern_expression(&clause, row)?;
        if let Some(predicate) = predicate {
            let mut filtered = Vec::new();
            for row in matches {
                if expression::predicate(self.evaluate(&predicate, &row)?)? {
                    filtered.push(row);
                }
            }
            matches = filtered;
        }
        self.expression_reservation
            .grow(crate::query::resources::allocation_bytes::<Value>(
                matches.len(),
            )?)?;
        let mut values = Vec::with_capacity(matches.len());
        for row in matches {
            values.push(self.evaluate(&projection, &row)?);
        }
        Ok(Value::List(values))
    }

    fn execute_pattern_expression(
        &mut self,
        clause: &AstNode,
        row: &BindingRow,
    ) -> QueryResult<(Vec<BindingRow>, MemoryReservation)> {
        let input = RowSet::try_clone_rows(&row.order, std::slice::from_ref(row), self.control)?;
        super::super::path::execute_match(
            &self.snapshot,
            self.graph_view,
            self.params,
            clause,
            input.rows,
            self.metrics,
            self.control,
        )
    }

    pub(super) fn evaluate_subquery_expression(
        &mut self,
        kind: crate::cypher::SubqueryKind,
        node: &AstNode,
        row: &BindingRow,
    ) -> QueryResult<Value> {
        let input = RowSet::try_clone_rows(&row.order, std::slice::from_ref(row), self.control)?;
        let mut globals =
            RowSet::try_clone_rows(&row.order, std::slice::from_ref(row), self.control)?;
        let global_row = globals
            .rows
            .pop()
            .ok_or_else(|| QueryError::internal("subquery globals lost their row"))?;
        let previous_globals = std::mem::replace(&mut self.global_bindings, global_row);
        let result = if let Some(body) = node
            .children
            .iter()
            .find(|child| child.kind == AstKind::QueryBody)
        {
            self.execute_query_body(body, input)
        } else {
            let clause = AstNode {
                kind: AstKind::Clause(ClauseKind::Match),
                span: node.span,
                text: None,
                children: node.children.clone(),
            };
            self.execute_match(&clause, false, input)
        };
        self.global_bindings = previous_globals;
        let result = result?;
        match kind {
            crate::cypher::SubqueryKind::Exists => Ok(Value::Boolean(!result.rows.is_empty())),
            crate::cypher::SubqueryKind::Count => Ok(Value::Integer(
                i64::try_from(result.rows.len()).map_err(|_| {
                    QueryError::new(
                        QueryErrorKind::Resource,
                        "COUNT subquery result is too large",
                    )
                })?,
            )),
            crate::cypher::SubqueryKind::Collect => self.collect_subquery_result(result),
            _ => Err(QueryError::internal(
                "CALL/braced subquery reached expression evaluation",
            )),
        }
    }
    fn collect_subquery_result(&mut self, result: RowSet) -> QueryResult<Value> {
        if result.columns.len() != 1 {
            return Err(QueryError::semantic(
                "COLLECT subquery must return exactly one column",
            ));
        }
        let column = &result.columns[0];
        self.expression_reservation
            .grow(crate::query::resources::allocation_bytes::<Value>(
                result.rows.len(),
            )?)?;
        let mut values = Vec::with_capacity(result.rows.len());
        for row in result.rows {
            self.control.work(1)?;
            let value = expression::binding_value_with_memory(
                &self.snapshot,
                row.values.get(column).unwrap_or(&BindingValue::Null),
                &self.graph_view.query_access,
                self.control,
            )?;
            self.expression_reservation.merge(value.reservation)?;
            values.push(value.value);
        }
        Ok(Value::List(values))
    }
}

fn pattern_comprehension_plan(
    node: &AstNode,
) -> QueryResult<(AstNode, expression::Expr, Option<expression::Expr>)> {
    let expressions = node
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::Expression(_)))
        .collect::<Vec<_>>();
    let projection = expressions
        .last()
        .ok_or_else(|| QueryError::semantic("pattern comprehension has no projection"))?;
    let predicate = (expressions.len() == 2).then_some(expressions[0]);
    if expressions.len() > 2 {
        return Err(QueryError::semantic(
            "pattern comprehension has an invalid expression shape",
        ));
    }
    let pattern = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::Pattern)
        .ok_or_else(|| QueryError::semantic("pattern comprehension has no pattern"))?;
    let assignment = node
        .children
        .iter()
        .find(|child| child.kind == AstKind::PathAssignment)
        .cloned();
    let clause = pattern_expression_clause(pattern, assignment)?;
    Ok((
        clause,
        compile_expression(projection)?,
        predicate.map(compile_expression).transpose()?,
    ))
}

pub(super) fn evaluate_lazy_graph_expression(
    expression: &expression::Expr,
    control: &ExecutionControl<'_>,
    evaluate: &mut impl FnMut(&expression::Expr) -> QueryResult<expression::EvaluatedValue>,
) -> QueryResult<Option<expression::EvaluatedValue>> {
    if matches!(
        expression,
        expression::Expr::Case { .. }
            | expression::Expr::Binary(expression::BinaryOp::And | expression::BinaryOp::Or, _, _)
    ) && expression_contains_graph_runtime(expression)
    {
        expression::evaluate_lazy_expression_with_memory(expression, evaluate, control)
    } else {
        Ok(None)
    }
}

pub(super) fn evaluate_scoped_graph_expression(
    expression: &expression::Expr,
    aliases: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
    evaluate: &mut impl FnMut(
        &expression::Expr,
        &BTreeMap<String, Value>,
    ) -> QueryResult<expression::EvaluatedValue>,
) -> QueryResult<Option<expression::EvaluatedValue>> {
    if matches!(
        expression,
        expression::Expr::ListComprehension { .. }
            | expression::Expr::ListPredicate { .. }
            | expression::Expr::Reduce { .. }
            | expression::Expr::AllReduce { .. }
    ) && expression_contains_graph_runtime(expression)
    {
        let mut memory = None;
        let value = expression::evaluate_scoped_expression_with_memory(
            expression,
            aliases,
            evaluate,
            control,
            &mut memory,
        )?;
        let reservation = match memory {
            Some(memory) => memory,
            None => {
                control.reserve(expression::value_bytes_with_control(&value, Some(control))?)?
            }
        };
        Ok(Some(expression::EvaluatedValue { value, reservation }))
    } else {
        Ok(None)
    }
}

pub(super) fn scoped_binding_row(
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    locals: &BTreeMap<String, Value>,
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let mut copied = RowSet::try_clone_rows(&row.order, std::slice::from_ref(row), control)?;
    let mut memory = copied
        .memory
        .take()
        .ok_or_else(|| QueryError::internal("scoped copy lost its memory reservation"))?;
    let mut local = copied
        .rows
        .pop()
        .ok_or_else(|| QueryError::internal("scoped copy lost its row"))?;
    for (name, value) in locals {
        control.work(1)?;
        memory.grow(expression::value_bytes_with_control(value, Some(control))?)?;
        local.insert(
            name.clone(),
            binding_from_value_controlled(
                snapshot,
                expression::clone_value_unreserved_controlled(value, Some(control))?,
                control,
            )?,
        );
    }
    Ok((local, memory))
}

fn expression_contains_graph_runtime(expression: &expression::Expr) -> bool {
    let mut found = false;
    let _ = expression::transform_expression(expression, &mut |candidate| {
        if matches!(
            candidate,
            expression::Expr::PatternPredicate(_)
                | expression::Expr::PatternComprehension(_)
                | expression::Expr::Subquery { .. }
        ) {
            found = true;
        }
        Ok(None)
    });
    found
}
