use crate::storage::{self, OwnerKind, RelationshipRecord};

use super::helpers::{
    cross_join_registry, function_registry_rows, procedure_registry_rows, project_named_columns,
    single_column_rows, yield_items,
};
use super::*;

fn resolve_procedure_call(
    clause: &AstNode,
) -> QueryResult<(&str, super::super::super::registry::ProcedureDefinition)> {
    let name_node = clause
        .descendants()
        .find(|node| node.kind == AstKind::FunctionName)
        .ok_or_else(|| QueryError::semantic("CALL is missing its procedure name"))?;
    let name = name_node
        .text
        .as_deref()
        .ok_or_else(|| QueryError::semantic("CALL is missing its procedure name"))?;
    let procedure = super::super::super::registry::procedure(name)
        .ok_or_else(|| QueryError::semantic(format!("unknown procedure {name}")))?;
    validate_procedure_arguments(clause, name_node, name)?;
    Ok((name, procedure))
}

fn validate_procedure_arguments(
    clause: &AstNode,
    name_node: &AstNode,
    name: &str,
) -> QueryResult<()> {
    let yield_start = clause
        .descendants()
        .filter(|node| matches!(node.kind, AstKind::YieldItem | AstKind::YieldAll))
        .map(|node| node.span.start)
        .min()
        .unwrap_or(clause.span.end);
    let has_arguments = clause.descendants().any(|node| {
        node.kind == AstKind::ArgumentList
            && node.span.start >= name_node.span.end
            && node.span.start < yield_start
    });
    if has_arguments {
        Err(QueryError::semantic(format!(
            "procedure {name} expects no arguments"
        )))
    } else {
        Ok(())
    }
}

fn resolved_yield_items(clause: &AstNode, outputs: &[&str]) -> Vec<(String, String)> {
    let mut items = yield_items(clause);
    if items.is_empty() {
        items.extend(
            outputs
                .iter()
                .map(|name| ((*name).to_owned(), (*name).to_owned())),
        );
    }
    items
}

fn procedure_columns(mut columns: Vec<String>, yield_items: &[(String, String)]) -> Vec<String> {
    for (_, output) in yield_items {
        if !columns.contains(output) {
            columns.push(output.clone());
        }
    }
    columns
}

fn show_registry_rows(clause: &AstNode) -> QueryResult<Vec<BindingRow>> {
    if clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::ShowTarget(crate::cypher::ShowTargetKind::Functions)
        )
    }) {
        return Ok(function_registry_rows());
    }
    if clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::ShowTarget(crate::cypher::ShowTargetKind::Procedures)
        )
    }) {
        return Ok(procedure_registry_rows());
    }
    Err(QueryError::semantic(
        "this SHOW surface is owned by a later Phase",
    ))
}

fn default_show_projection(
    mut result: RowSet,
    mut columns: Vec<String>,
    default_columns: Vec<String>,
) -> RowSet {
    for column in default_columns {
        if !columns.contains(&column) {
            columns.push(column);
        }
    }
    result.rows = result
        .rows
        .into_iter()
        .map(|row| project_named_columns(row, &columns))
        .collect();
    result.columns = columns;
    result
}

impl ReadExecutor<'_, '_> {
    pub(super) fn execute_procedure(
        &mut self,
        clause: &AstNode,
        input: RowSet,
    ) -> QueryResult<RowSet> {
        let (name, procedure) = resolve_procedure_call(clause)?;
        let procedure_rows = self.current_graph_procedure(name)?;
        let yield_items = resolved_yield_items(clause, procedure.outputs);
        let columns = procedure_columns(input.columns.clone(), &yield_items);
        let rows = self.join_procedure_rows(input.rows, &procedure_rows, &yield_items, name)?;
        let rows = self.filter_yield_rows(clause, rows)?;
        Ok(RowSet { columns, rows })
    }

    fn join_procedure_rows(
        &self,
        input_rows: Vec<BindingRow>,
        procedure_rows: &[BTreeMap<String, Value>],
        yield_items: &[(String, String)],
        name: &str,
    ) -> QueryResult<Vec<BindingRow>> {
        let mut rows = Vec::new();
        for input in input_rows {
            for procedure_row in procedure_rows {
                rows.push(self.join_procedure_row(&input, procedure_row, yield_items, name)?);
            }
        }
        Ok(rows)
    }

    fn join_procedure_row(
        &self,
        input: &BindingRow,
        procedure_row: &BTreeMap<String, Value>,
        yield_items: &[(String, String)],
        name: &str,
    ) -> QueryResult<BindingRow> {
        let mut row = input.clone();
        for (source, output) in yield_items {
            let value = procedure_row.get(source).cloned().ok_or_else(|| {
                QueryError::semantic(format!(
                    "procedure {name} does not yield output field {source:?}"
                ))
            })?;
            row.insert(output.clone(), binding_from_value(&self.snapshot, value)?);
        }
        Ok(row)
    }

    fn filter_yield_rows(
        &mut self,
        clause: &AstNode,
        rows: Vec<BindingRow>,
    ) -> QueryResult<Vec<BindingRow>> {
        let Some(where_clause) = clause
            .descendants()
            .find(|node| node.kind == AstKind::Where)
        else {
            return Ok(rows);
        };
        let predicate = surface_expressions(where_clause)
            .into_iter()
            .next()
            .ok_or_else(|| QueryError::semantic("YIELD WHERE has no predicate"))?;
        self.filter_rows(&compile_expression(predicate)?, rows)
    }

    fn current_graph_procedure(&self, name: &str) -> QueryResult<Vec<BTreeMap<String, Value>>> {
        if super::super::super::registry::procedure(name).is_none() {
            return Err(QueryError::semantic(format!("unknown procedure {name}")));
        }
        let (nodes, relationships) = self.current_graph_entities()?;
        match name.to_ascii_lowercase().as_str() {
            "db.labels" => self.current_labels(nodes),
            "db.relationshiptypes" => self.current_relationship_types(relationships),
            "db.propertykeys" => self.current_property_keys(nodes, relationships),
            _ => Err(QueryError::internal(format!(
                "registered procedure {name} has no executor"
            ))),
        }
    }

    fn current_graph_entities(&self) -> QueryResult<(Vec<i64>, Vec<RelationshipRecord>)> {
        let mut nodes = Vec::new();
        self.snapshot.visit_nodes(|node| {
            nodes.push(node);
            Ok(())
        })?;
        let mut relationships = Vec::new();
        self.snapshot.visit_relationships(|relationship| {
            relationships.push(relationship);
            Ok(())
        })?;
        Ok((nodes, relationships))
    }

    fn current_labels(&self, nodes: Vec<i64>) -> QueryResult<Vec<BTreeMap<String, Value>>> {
        let mut values = BTreeSet::new();
        for node in nodes {
            if self.graph_view.visible_node(&self.snapshot, node)? {
                for label in self.snapshot.labels(node)? {
                    if let Some(name) = storage::label_name(self.connection, label)? {
                        values.insert(name);
                    }
                }
            }
        }
        Ok(single_column_rows("label", values))
    }

    fn current_relationship_types(
        &self,
        relationships: Vec<RelationshipRecord>,
    ) -> QueryResult<Vec<BTreeMap<String, Value>>> {
        let mut values = BTreeSet::new();
        for relationship in relationships {
            if self
                .graph_view
                .visible_relationship(&self.snapshot, relationship)?
                && let Some(name) =
                    storage::relationship_type_name(self.connection, relationship.type_id)?
            {
                values.insert(name);
            }
        }
        Ok(single_column_rows("relationshipType", values))
    }

    fn current_property_keys(
        &self,
        nodes: Vec<i64>,
        relationships: Vec<RelationshipRecord>,
    ) -> QueryResult<Vec<BTreeMap<String, Value>>> {
        let mut values = BTreeSet::new();
        for node in nodes {
            if self.graph_view.visible_node(&self.snapshot, node)? {
                for (key, _) in self.snapshot.properties(OwnerKind::Node, node)? {
                    if let Some(name) = storage::property_key_name(self.connection, key)? {
                        values.insert(name);
                    }
                }
            }
        }
        for relationship in relationships {
            if self
                .graph_view
                .visible_relationship(&self.snapshot, relationship)?
            {
                for (key, _) in self
                    .snapshot
                    .properties(OwnerKind::Relationship, relationship.id)?
                {
                    if let Some(name) = storage::property_key_name(self.connection, key)? {
                        values.insert(name);
                    }
                }
            }
        }
        Ok(single_column_rows("propertyKey", values))
    }

    pub(super) fn execute_show(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let all_columns = super::super::show_all_columns(clause)?;
        let default_columns = super::super::show_default_columns(clause)?;
        let yield_node = crate::cypher::show_yield_node(clause);
        let mut registry = RowSet {
            columns: all_columns,
            rows: show_registry_rows(clause)?,
        };
        if let Some(yield_node) = yield_node {
            registry = self.execute_projection(
                &crate::cypher::show_projection_clause(yield_node),
                registry,
                false,
            )?;
        }
        let input_columns = input.columns.clone();
        let mut result = cross_join_registry(input, registry);
        result.rows = self.filter_show_rows(clause, result.rows)?;
        if let Some(return_clause) = clause
            .children
            .iter()
            .find(|node| node.kind == AstKind::Clause(ClauseKind::Return))
        {
            return self.execute_projection(return_clause, result, false);
        }
        if yield_node.is_none() {
            result = default_show_projection(result, input_columns, default_columns);
        }
        Ok(result)
    }

    fn filter_show_rows(
        &mut self,
        clause: &AstNode,
        rows: Vec<BindingRow>,
    ) -> QueryResult<Vec<BindingRow>> {
        let Some(where_node) = clause
            .children
            .iter()
            .find(|node| node.kind == AstKind::Where)
        else {
            return Ok(rows);
        };
        let predicate = surface_expressions(where_node)
            .into_iter()
            .next()
            .ok_or_else(|| QueryError::semantic("SHOW WHERE has no predicate"))?;
        self.filter_rows(&compile_expression(predicate)?, rows)
    }
}
