use super::*;

impl ReadExecutor<'_, '_> {
    pub(super) fn materialize_subqueries(
        &mut self,
        value: &expression::Expr,
        row: &BindingRow,
        aliases: &BTreeMap<String, Value>,
    ) -> QueryResult<expression::Expr> {
        expression::transform_expression(value, &mut |candidate| {
            if let Some(value) = evaluate_lazy_graph_expression(candidate, &mut |nested| {
                self.evaluate_with_aliases(nested, row, aliases)
            })? {
                return Ok(Some(expression::Expr::Literal(value)));
            }
            if let Some(value) =
                evaluate_scoped_graph_expression(candidate, aliases, &mut |nested, locals| {
                    let local = scoped_binding_row(&self.snapshot, row, locals)?;
                    self.evaluate_with_aliases(nested, &local, locals)
                })?
            {
                return Ok(Some(expression::Expr::Literal(value)));
            }
            Ok(self
                .evaluate_graph_runtime_candidate(candidate, row, aliases)?
                .map(expression::Expr::Literal))
        })
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
        let row = if aliases.is_empty() {
            row
        } else {
            local = scoped_binding_row(&self.snapshot, row, aliases)?;
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
        let matches = self.execute_pattern_expression(&clause, row)?;
        Ok(Value::Boolean(!matches.is_empty()))
    }

    pub(super) fn evaluate_pattern_comprehension(
        &mut self,
        node: &AstNode,
        row: &BindingRow,
    ) -> QueryResult<Value> {
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
        let mut matches = self.execute_pattern_expression(&clause, row)?;
        if let Some(predicate) = predicate {
            let predicate = compile_expression(predicate)?;
            let mut filtered = Vec::new();
            for row in matches {
                if expression::predicate(self.evaluate(&predicate, &row)?)? {
                    filtered.push(row);
                }
            }
            matches = filtered;
        }
        let projection = compile_expression(projection)?;
        matches
            .into_iter()
            .map(|row| self.evaluate(&projection, &row))
            .collect::<QueryResult<Vec<_>>>()
            .map(Value::List)
    }

    fn execute_pattern_expression(
        &mut self,
        clause: &AstNode,
        row: &BindingRow,
    ) -> QueryResult<Vec<BindingRow>> {
        super::super::path::execute_match(
            &self.snapshot,
            self.graph_view,
            self.params,
            clause,
            vec![row.clone()],
            self.metrics,
            self.is_interrupted,
        )
    }

    pub(super) fn evaluate_subquery_expression(
        &mut self,
        kind: crate::cypher::SubqueryKind,
        node: &AstNode,
        row: &BindingRow,
    ) -> QueryResult<Value> {
        let input = RowSet {
            columns: row.order.clone(),
            rows: vec![row.clone()],
        };
        let previous_globals = std::mem::replace(&mut self.global_bindings, row.clone());
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
            crate::cypher::SubqueryKind::Collect => {
                if result.columns.len() != 1 {
                    return Err(QueryError::semantic(
                        "COLLECT subquery must return exactly one column",
                    ));
                }
                let column = &result.columns[0];
                result
                    .rows
                    .into_iter()
                    .map(|row| {
                        binding_value_with_access(
                            &self.snapshot,
                            row.values.get(column).unwrap_or(&BindingValue::Null),
                            &self.graph_view.query_access,
                        )
                    })
                    .collect::<QueryResult<Vec<_>>>()
                    .map(Value::List)
            }
            _ => Err(QueryError::internal(
                "CALL/braced subquery reached expression evaluation",
            )),
        }
    }
}

pub(super) fn evaluate_lazy_graph_expression(
    expression: &expression::Expr,
    evaluate: &mut impl FnMut(&expression::Expr) -> QueryResult<Value>,
) -> QueryResult<Option<Value>> {
    if matches!(
        expression,
        expression::Expr::Case { .. }
            | expression::Expr::Binary(expression::BinaryOp::And | expression::BinaryOp::Or, _, _)
    ) && expression_contains_graph_runtime(expression)
    {
        expression::evaluate_lazy_expression(expression, evaluate)
    } else {
        Ok(None)
    }
}

pub(super) fn evaluate_scoped_graph_expression(
    expression: &expression::Expr,
    aliases: &BTreeMap<String, Value>,
    evaluate: &mut impl FnMut(&expression::Expr, &BTreeMap<String, Value>) -> QueryResult<Value>,
) -> QueryResult<Option<Value>> {
    if matches!(
        expression,
        expression::Expr::ListComprehension { .. }
            | expression::Expr::ListPredicate { .. }
            | expression::Expr::Reduce { .. }
            | expression::Expr::AllReduce { .. }
    ) && expression_contains_graph_runtime(expression)
    {
        expression::evaluate_scoped_expression(expression, aliases, evaluate).map(Some)
    } else {
        Ok(None)
    }
}

pub(super) fn scoped_binding_row(
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    locals: &BTreeMap<String, Value>,
) -> QueryResult<BindingRow> {
    let mut local = row.clone();
    for (name, value) in locals {
        local.insert(name.clone(), binding_from_value(snapshot, value.clone())?);
    }
    Ok(local)
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
