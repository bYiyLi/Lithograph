use super::super::explicit_imports;
use super::*;
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};

impl ReadExecutor<'_, '_> {
    pub(super) fn execute_call(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        if let Some(subquery) = crate::query::completeness::call_subquery(clause) {
            return self.execute_call_subquery(subquery, input);
        }
        self.execute_procedure(clause, input)
    }

    fn execute_call_subquery(&mut self, subquery: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let body = required_query_body(subquery, "CALL subquery is missing its query body")?;
        let scope = crate::query::completeness::subquery_scope(subquery);
        let imports = explicit_imports(scope, &input.columns);
        let returns_rows = crate::cypher::query_body_returns_columns(body);
        let mut output_columns = input.columns.clone();
        let mut output_rows = Vec::new();
        let mut memory = self.control.reserve(0)?;
        for outer in input.rows {
            self.control.checkpoint(1)?;
            let result = self.execute_call_for_outer(body, scope.is_some(), &imports, &outer)?;
            let mut merged = merge_call_result(&outer, result, returns_rows, self.control)?;
            append_call_columns(
                &mut output_columns,
                &merged.columns,
                &mut memory,
                self.control,
            )?;
            if let Some(merged_memory) = merged.memory.take() {
                memory.merge(merged_memory)?;
            }
            output_rows.extend(merged.rows);
        }
        Ok(RowSet {
            columns: output_columns,
            rows: output_rows,
            memory: Some(memory),
        })
    }

    fn execute_call_for_outer(
        &mut self,
        body: &AstNode,
        scoped: bool,
        imports: &[String],
        outer: &BindingRow,
    ) -> QueryResult<RowSet> {
        if !scoped {
            return self.execute_call_body_with_importing_with(body, outer);
        }
        let imported = project_call_bindings(outer, imports, self.control)?;
        let mut globals = imported.try_clone(self.control)?;
        let global_row = globals
            .rows
            .pop()
            .ok_or_else(|| QueryError::internal("CALL global binding seed is missing"))?;
        // globals.memory stays alive while the copied row is installed in the
        // executor, including failure paths through the nested query body.
        let previous_globals = std::mem::replace(&mut self.global_bindings, global_row);
        let result = self.execute_query_body(body, imported);
        self.global_bindings = previous_globals;
        drop(globals);
        result
    }

    fn execute_call_body_with_importing_with(
        &mut self,
        body: &AstNode,
        outer: &BindingRow,
    ) -> QueryResult<RowSet> {
        let query = executable_query(body, true)?;
        self.in_expression_scope(|executor| {
            executor.execute_call_query_with_importing_with(query, outer)
        })
    }

    fn execute_call_query_with_importing_with(
        &mut self,
        query: &AstNode,
        outer: &BindingRow,
    ) -> QueryResult<RowSet> {
        match query.kind {
            AstKind::SingleQuery => {
                let seed = importing_with_seed(query, outer, self.control)?;
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
                    result =
                        self.execute_operand(operand, clone_row_set(&next_input, self.control)?)?;
                    segment_input = Some(next_input);
                }
                QueryConnector::Union
                | QueryConnector::UnionAll
                | QueryConnector::UnionDistinct => {
                    let right = if let Some(input) = &segment_input {
                        self.execute_operand(operand, clone_row_set(input, self.control)?)?
                    } else {
                        self.execute_call_query_with_importing_with(operand, outer)?
                    };
                    result = union_rows(
                        result,
                        right,
                        connector != QueryConnector::UnionAll,
                        &self.snapshot,
                        &self.graph_view.query_access,
                        self.control,
                    )?;
                }
            }
            previous_operand = operand;
        }
        Ok(result)
    }
}

fn importing_with_seed(
    single: &AstNode,
    outer: &BindingRow,
    control: &ExecutionControl<'_>,
) -> QueryResult<RowSet> {
    let columns = super::super::importing_with_columns(single, &outer.order);
    project_call_bindings(outer, &columns, control)
}

pub(crate) fn project_call_bindings(
    outer: &BindingRow,
    columns: &[String],
    control: &ExecutionControl<'_>,
) -> QueryResult<RowSet> {
    let memory = control.reserve(call_import_bytes(outer, columns, control)?)?;
    let mut row = BindingRow::default();
    for name in columns {
        control.checkpoint(1)?;
        let value = outer
            .values
            .get(name)
            .map(|value| {
                expression::clone_binding_value_unreserved_controlled(value, Some(control))
            })
            .transpose()?
            .unwrap_or(BindingValue::Null);
        row.insert(
            expression::clone_string_controlled(name, Some(control))?,
            value,
        );
    }
    row.load_csv_context =
        expression::clone_load_csv_context_controlled(&outer.load_csv_context, Some(control))?;
    Ok(RowSet {
        columns: columns.to_vec(),
        rows: vec![row],
        memory: Some(memory),
    })
}

fn call_import_bytes(
    outer: &BindingRow,
    columns: &[String],
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let mut bytes = checked_add(
        allocation_bytes::<BindingRow>(2)?,
        allocation_bytes::<String>(columns.len().saturating_mul(3).saturating_add(4))?,
    )?;
    for name in columns {
        control.checkpoint(1)?;
        let value_bytes = outer
            .values
            .get(name)
            .map(|value| expression::binding_value_bytes(value, Some(control)))
            .transpose()?
            .unwrap_or(0);
        bytes = checked_add(
            bytes,
            checked_add(
                map_entry_bytes::<BindingValue>()?,
                checked_add(value_bytes, checked_mul(name.len() as u64, 3)?)?,
            )?,
        )?;
    }
    checked_add(bytes, call_context_bytes(outer))
}

fn call_context_bytes(row: &BindingRow) -> u64 {
    row.load_csv_context
        .as_ref()
        .and_then(|context| context.file.as_ref())
        .map_or(0, |file| file.capacity() as u64)
}

pub(crate) fn append_call_columns(
    output: &mut Vec<String>,
    columns: &[String],
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    for column in columns {
        control.checkpoint(1)?;
        if !output.contains(column) {
            memory.grow(checked_add(
                allocation_bytes::<String>(2)?,
                column.len() as u64,
            )?)?;
            output.push(expression::clone_string_controlled(column, Some(control))?);
        }
    }
    Ok(())
}

pub(crate) fn merge_call_result(
    outer: &BindingRow,
    result: RowSet,
    returns_rows: bool,
    control: &ExecutionControl<'_>,
) -> QueryResult<RowSet> {
    let count = if returns_rows { result.rows.len() } else { 1 };
    let mut memory = control.reserve(allocation_bytes::<BindingRow>(count.saturating_mul(2))?)?;
    let mut rows = Vec::with_capacity(count);
    if !returns_rows {
        let row_memory = control.reserve(expression::binding_row_bytes_controlled(
            outer,
            Some(control),
        )?)?;
        rows.push(expression::clone_binding_row_unreserved_controlled(
            outer,
            Some(control),
        )?);
        memory.merge(row_memory)?;
    } else {
        for inner in result.rows {
            control.checkpoint(1)?;
            let (row, row_memory) = combine_call_row(outer, &inner, &result.columns, control)?;
            memory.merge(row_memory)?;
            rows.push(row);
        }
    }
    Ok(RowSet {
        columns: result.columns,
        rows,
        memory: Some(memory),
    })
}

fn combine_call_row(
    outer: &BindingRow,
    inner: &BindingRow,
    columns: &[String],
    control: &ExecutionControl<'_>,
) -> QueryResult<(BindingRow, MemoryReservation)> {
    let bytes = checked_add(
        expression::binding_row_bytes_controlled(outer, Some(control))?,
        call_import_bytes(inner, columns, control)?,
    )?;
    let memory = control.reserve(bytes)?;
    let mut combined = expression::clone_binding_row_unreserved_controlled(outer, Some(control))?;
    for column in columns {
        control.checkpoint(1)?;
        let value = inner
            .values
            .get(column)
            .map(|value| {
                expression::clone_binding_value_unreserved_controlled(value, Some(control))
            })
            .transpose()?
            .unwrap_or(BindingValue::Null);
        combined.insert(
            expression::clone_string_controlled(column, Some(control))?,
            value,
        );
    }
    if inner.load_csv_context.is_some() {
        combined.load_csv_context =
            expression::clone_load_csv_context_controlled(&inner.load_csv_context, Some(control))?;
    }
    Ok((combined, memory))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::query::resources::QueryResources;

    #[test]
    fn call_imports_only_copy_selected_bindings() {
        let resources = QueryResources::new(Some(16 * 1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut outer = BindingRow::default();
        outer.insert(
            "selected".to_owned(),
            BindingValue::Scalar(Value::Integer(42)),
        );
        outer.insert(
            "unused".to_owned(),
            BindingValue::Scalar(Value::String("x".repeat(512 * 1024))),
        );
        let rows = project_call_bindings(
            &outer,
            &["selected".to_owned(), "missing".to_owned()],
            &control,
        )
        .expect("small selected import fits");
        assert_eq!(
            rows.rows[0].values.get("selected"),
            Some(&BindingValue::Scalar(Value::Integer(42)))
        );
        assert_eq!(
            rows.rows[0].values.get("missing"),
            Some(&BindingValue::Null)
        );
        assert!(!rows.rows[0].values.contains_key("unused"));
        assert!(resources.used() > 0);
        drop(rows);
        assert_eq!(resources.used(), 0);
    }

    #[test]
    fn call_import_copy_checks_budget_before_materializing_values() {
        let resources = QueryResources::new(Some(1024));
        let control = ExecutionControl::new(&resources, &|| false);
        let mut outer = BindingRow::default();
        outer.insert(
            "large".to_owned(),
            BindingValue::Scalar(Value::List((0..10_000).map(Value::Integer).collect())),
        );
        let error = project_call_bindings(&outer, &["large".to_owned()], &control)
            .err()
            .expect("large copy exceeds budget");
        assert_eq!(error.kind, QueryErrorKind::Resource);
        assert_eq!(resources.used(), 0);
        assert_eq!(resources.peak(), 0);
    }

    #[test]
    fn unit_call_retains_one_outer_row_with_its_memory() {
        let resources = QueryResources::default();
        let control = ExecutionControl::new(&resources, &|| false);
        let mut outer = BindingRow::default();
        outer.insert(
            "x".to_owned(),
            BindingValue::Scalar(Value::String("payload".to_owned())),
        );
        let result = merge_call_result(
            &outer,
            RowSet {
                columns: Vec::new(),
                rows: Vec::new(),
                memory: None,
            },
            false,
            &control,
        )
        .expect("unit CALL preserves its outer row");
        assert_eq!(result.rows, vec![outer]);
        assert!(
            result
                .memory
                .as_ref()
                .is_some_and(|memory| memory.bytes() > 0)
        );
        drop(result);
        assert_eq!(resources.used(), 0);
    }
}
