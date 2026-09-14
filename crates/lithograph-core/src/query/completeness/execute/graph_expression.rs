use super::*;

impl ReadExecutor<'_, '_> {
    pub(super) fn materialize_subqueries(
        &mut self,
        value: &expression::Expr,
        row: &BindingRow,
    ) -> QueryResult<expression::Expr> {
        expression::transform_expression(value, &mut |candidate| {
            Ok(self
                .evaluate_graph_runtime_candidate(candidate, row)?
                .map(expression::Expr::Literal))
        })
    }

    pub(super) fn evaluate_graph_runtime_candidate(
        &mut self,
        candidate: &expression::Expr,
        row: &BindingRow,
    ) -> QueryResult<Option<Value>> {
        use expression::Expr;
        let value = match candidate {
            Expr::Subquery { kind, node } => self.evaluate_subquery_expression(*kind, node, row)?,
            Expr::ListComprehension {
                variable,
                collection,
                predicate,
                projection,
            } if list_comprehension_needs_graph_runtime(
                predicate.as_deref(),
                projection.as_deref(),
            ) =>
            {
                self.evaluate_list_comprehension_expression(
                    variable,
                    collection,
                    predicate.as_deref(),
                    projection.as_deref(),
                    row,
                )?
            }
            Expr::PatternPredicate(node) => self.evaluate_pattern_predicate(node, row)?,
            Expr::PatternComprehension(node) => self.evaluate_pattern_comprehension(node, row)?,
            _ => return Ok(None),
        };
        Ok(Some(value))
    }

    pub(super) fn evaluate_list_comprehension_expression(
        &mut self,
        variable: &str,
        collection: &expression::Expr,
        predicate: Option<&expression::Expr>,
        projection: Option<&expression::Expr>,
        row: &BindingRow,
    ) -> QueryResult<Value> {
        let collection = self.evaluate(collection, row)?;
        let Value::List(values) = collection else {
            if matches!(collection, Value::Null) {
                return Ok(Value::Null);
            }
            return Err(QueryError::new(
                QueryErrorKind::Type,
                "list comprehension requires a List or null",
            ));
        };
        let mut output = Vec::new();
        for value in values {
            let mut local = row.clone();
            local.insert(
                variable.to_owned(),
                binding_from_value(&self.snapshot, value.clone())?,
            );
            let keep = match predicate {
                None => true,
                Some(predicate) => expression::predicate(self.evaluate(predicate, &local)?)?,
            };
            if keep {
                output.push(match projection {
                    Some(projection) => self.evaluate(projection, &local)?,
                    None => value,
                });
            }
        }
        Ok(Value::List(output))
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
                        binding_value(
                            &self.snapshot,
                            row.values.get(column).unwrap_or(&BindingValue::Null),
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

pub(super) fn list_comprehension_needs_graph_runtime(
    predicate: Option<&expression::Expr>,
    projection: Option<&expression::Expr>,
) -> bool {
    predicate.is_some_and(expression_contains_graph_runtime)
        || projection.is_some_and(expression_contains_graph_runtime)
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
