use super::super::{explicit_imports, project_bindings};
use super::*;

impl ReadExecutor<'_, '_> {
    pub(super) fn execute_call(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        if let Some(subquery) = clause.children.iter().find(|node| {
            matches!(
                node.kind,
                AstKind::Subquery(crate::cypher::SubqueryKind::Call)
            )
        }) {
            return self.execute_call_subquery(subquery, input);
        }
        self.execute_procedure(clause, input)
    }

    fn execute_call_subquery(&mut self, subquery: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let body = required_query_body(subquery, "CALL subquery is missing its query body")?;
        let scope = subquery
            .children
            .iter()
            .find(|node| node.kind == AstKind::SubqueryScope);
        let imports = explicit_imports(scope, &input.columns);
        let returns_rows = crate::cypher::query_body_returns_columns(body);
        let mut output_columns = input.columns.clone();
        let mut output_rows = Vec::new();
        for outer in input.rows {
            self.check_interrupted()?;
            let imported = project_bindings(&outer, &imports);
            let globals = scope.map_or_else(BindingRow::default, |_| imported.clone());
            let previous_globals = std::mem::replace(&mut self.global_bindings, globals);
            let result = if scope.is_some() {
                self.execute_query_body(
                    body,
                    RowSet {
                        columns: imported.order.clone(),
                        rows: vec![imported],
                    },
                )
            } else {
                self.execute_call_body_with_importing_with(body, &outer)
            };
            self.global_bindings = previous_globals;
            let result = result?;
            merge_call_result(
                &outer,
                result,
                returns_rows,
                &mut output_columns,
                &mut output_rows,
            );
        }
        Ok(RowSet {
            columns: output_columns,
            rows: output_rows,
        })
    }

    fn execute_call_body_with_importing_with(
        &mut self,
        body: &AstNode,
        outer: &BindingRow,
    ) -> QueryResult<RowSet> {
        let query = executable_query(body, true)?;
        self.execute_call_query_with_importing_with(query, outer)
    }

    fn execute_call_query_with_importing_with(
        &mut self,
        query: &AstNode,
        outer: &BindingRow,
    ) -> QueryResult<RowSet> {
        match query.kind {
            AstKind::SingleQuery => {
                let seed = importing_with_seed(query, outer);
                self.execute_single(query, seed)
            }
            AstKind::Subquery(crate::cypher::SubqueryKind::Braced) => {
                let body = required_query_body(query, "braced query is missing its body")?;
                self.execute_call_body_with_importing_with(body, outer)
            }
            AstKind::ComposedQuery => self.execute_call_composed(query, outer),
            AstKind::ConditionalQuery => self.execute_conditional(query, RowSet::seed()),
            _ => Err(QueryError::internal("invalid CALL subquery query body")),
        }
    }

    fn execute_call_composed(
        &mut self,
        query: &AstNode,
        outer: &BindingRow,
    ) -> QueryResult<RowSet> {
        let parts = composed_query_parts(query)?;
        let mut result = self.execute_call_query_with_importing_with(parts.first, outer)?;
        let mut segment_input = None;
        let mut previous_operand = parts.first;
        for (connector, operand) in parts.rest {
            self.check_interrupted()?;
            match connector {
                QueryConnector::Next => {
                    let next_input = rows_for_next(previous_operand, result);
                    result = self.execute_operand(operand, clone_row_set(&next_input))?;
                    segment_input = Some(next_input);
                }
                QueryConnector::Union
                | QueryConnector::UnionAll
                | QueryConnector::UnionDistinct => {
                    let right = if let Some(input) = &segment_input {
                        self.execute_operand(operand, clone_row_set(input))?
                    } else {
                        self.execute_call_query_with_importing_with(operand, outer)?
                    };
                    result = union_rows(
                        result,
                        right,
                        connector != QueryConnector::UnionAll,
                        &self.snapshot,
                    )?;
                }
            }
            previous_operand = operand;
        }
        Ok(result)
    }
}

fn importing_with_seed(single: &AstNode, outer: &BindingRow) -> RowSet {
    let columns = super::super::importing_with_columns(single, &outer.order);
    let row = project_bindings(outer, &columns);
    RowSet {
        columns,
        rows: vec![row],
    }
}

fn merge_call_result(
    outer: &BindingRow,
    result: RowSet,
    returns_rows: bool,
    output_columns: &mut Vec<String>,
    output_rows: &mut Vec<BindingRow>,
) {
    for column in &result.columns {
        if !output_columns.contains(column) {
            output_columns.push(column.clone());
        }
    }
    if !returns_rows {
        output_rows.push(outer.clone());
        return;
    }
    for inner in result.rows {
        let mut combined = outer.clone();
        for column in &result.columns {
            combined.insert(
                column.clone(),
                inner
                    .values
                    .get(column)
                    .cloned()
                    .unwrap_or(BindingValue::Null),
            );
        }
        output_rows.push(combined);
    }
}
