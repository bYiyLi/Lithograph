use crate::storage::{self, OwnerKind, RelationshipRecord, StandardIndexKind};

use super::super::super::semantic_index::{
    FullTextQueryInput, SemanticEntity, fulltext_query, resolve_semantic_index,
};

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
    let arguments = clause.descendants().find(|node| {
        node.kind == AstKind::ArgumentList
            && node.span.start >= name_node.span.end
            && node.span.start < yield_start
    });
    let count = arguments.map_or(0, |arguments| surface_expressions(arguments).len());
    if is_fulltext_procedure(name) {
        if (2..=3).contains(&count) {
            return Ok(());
        }
        return Err(QueryError::semantic(format!(
            "procedure {name} expects 2 or 3 arguments"
        )));
    }
    if let Some((minimum, maximum)) = version_argument_range(name) {
        if (minimum..=maximum).contains(&count) {
            return Ok(());
        }
        return Err(QueryError::semantic(format!(
            "procedure {name} expects {}",
            if minimum == maximum {
                minimum.to_string()
            } else {
                format!("between {minimum} and {maximum}")
            }
        )));
    }
    if arguments.is_some() {
        Err(QueryError::semantic(format!(
            "procedure {name} expects no arguments"
        )))
    } else {
        Ok(())
    }
}

fn is_fulltext_procedure(name: &str) -> bool {
    matches!(
        name.to_ascii_lowercase().as_str(),
        "db.index.fulltext.querynodes" | "db.index.fulltext.queryrelationships"
    )
}

fn version_argument_range(name: &str) -> Option<(usize, usize)> {
    Some(match name.to_ascii_lowercase().as_str() {
        "lithograph.branch.create" => (1, 2),
        "lithograph.branch.checkout" | "lithograph.branch.delete" => (1, 1),
        "lithograph.branch.list" | "lithograph.tag.list" | "lithograph.gc" => (0, 0),
        "lithograph.commit.get"
        | "lithograph.commit.data.clear"
        | "lithograph.tag.delete"
        | "lithograph.patch.apply"
        | "lithograph.merge.get"
        | "lithograph.squash"
        | "lithograph.reset" => (1, 1),
        "lithograph.commit.create" => (0, 1),
        "lithograph.commit.data.set"
        | "lithograph.tag.create"
        | "lithograph.tag.move"
        | "lithograph.diff"
        | "lithograph.merge.finalize"
        | "lithograph.merge.abort" => (2, 2),
        "lithograph.log" => (0, 3),
        "lithograph.merge.start" | "lithograph.rebase" | "lithograph.revert" => (1, 2),
        "lithograph.merge.list" => (0, 2),
        "lithograph.merge.conflicts" => (1, 3),
        "lithograph.merge.resolve" => (3, 3),
        _ => return None,
    })
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

fn show_registry_rows(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    clause: &AstNode,
) -> QueryResult<Vec<BindingRow>> {
    if clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::ShowTarget(crate::cypher::ShowTargetKind::Functions)
        )
    }) {
        return function_registry_rows();
    }
    if clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::ShowTarget(crate::cypher::ShowTargetKind::Procedures)
        )
    }) {
        return Ok(procedure_registry_rows());
    }
    super::super::super::schema::show_rows(connection, snapshot, clause)
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

#[derive(Default)]
struct FullTextQueryOptions {
    skip: usize,
    limit: Option<usize>,
    analyzer: Option<String>,
}

fn require_string_argument(value: Value, procedure: &str, argument: &str) -> QueryResult<String> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err(QueryError::semantic(format!(
            "procedure {procedure} argument {argument} must be String"
        ))),
    }
}

fn parse_fulltext_options(value: Value) -> QueryResult<FullTextQueryOptions> {
    let Value::Map(mut options) = value else {
        return Err(QueryError::semantic(
            "full-text query options must be a Map",
        ));
    };
    let skip = take_nonnegative_option(&mut options, "skip")?.unwrap_or(0);
    let limit = take_nonnegative_option(&mut options, "limit")?;
    let analyzer = match options.remove("analyzer") {
        None => None,
        Some(Value::String(value)) => Some(value),
        Some(_) => {
            return Err(QueryError::semantic(
                "full-text option analyzer must be String",
            ));
        }
    };
    if let Some(key) = options.keys().next() {
        return Err(QueryError::semantic(format!(
            "unsupported full-text query option {key:?}"
        )));
    }
    Ok(FullTextQueryOptions {
        skip,
        limit,
        analyzer,
    })
}

fn take_nonnegative_option(
    options: &mut BTreeMap<String, Value>,
    key: &str,
) -> QueryResult<Option<usize>> {
    let Some(value) = options.remove(key) else {
        return Ok(None);
    };
    let Value::Integer(value) = value else {
        return Err(QueryError::semantic(format!(
            "full-text option {key} must be Integer"
        )));
    };
    usize::try_from(value)
        .map(Some)
        .map_err(|_| QueryError::semantic(format!("full-text option {key} cannot be negative")))
}

fn procedure_argument_expressions(clause: &AstNode) -> QueryResult<Vec<expression::Expr>> {
    let Some(arguments) = clause
        .descendants()
        .find(|node| node.kind == AstKind::ArgumentList)
    else {
        return Ok(Vec::new());
    };
    surface_expressions(arguments)
        .into_iter()
        .map(compile_expression)
        .collect()
}

fn fulltext_argument_expressions(
    clause: &AstNode,
    name: &str,
) -> QueryResult<Vec<expression::Expr>> {
    let arguments = clause
        .descendants()
        .find(|node| node.kind == AstKind::ArgumentList)
        .ok_or_else(|| QueryError::semantic(format!("procedure {name} is missing arguments")))?;
    surface_expressions(arguments)
        .into_iter()
        .map(compile_expression)
        .collect()
}

impl ReadExecutor<'_, '_> {
    pub(super) fn execute_procedure(
        &mut self,
        clause: &AstNode,
        input: RowSet,
    ) -> QueryResult<RowSet> {
        let (name, procedure) = resolve_procedure_call(clause)?;
        if super::super::super::registry::is_version_procedure(name) {
            return self.execute_version_procedure(clause, input, name, procedure);
        }
        if is_fulltext_procedure(name) {
            return self.execute_fulltext_procedure(clause, input, name, procedure);
        }
        let procedure_rows = self.current_graph_procedure(name)?;
        let yield_items = resolved_yield_items(clause, procedure.outputs);
        let columns = procedure_columns(input.columns.clone(), &yield_items);
        let rows = self.join_procedure_rows(input.rows, &procedure_rows, &yield_items, name)?;
        let rows = self.filter_yield_rows(clause, rows)?;
        Ok(RowSet { columns, rows })
    }

    fn execute_version_procedure(
        &mut self,
        clause: &AstNode,
        input: RowSet,
        name: &str,
        procedure: super::super::super::registry::ProcedureDefinition,
    ) -> QueryResult<RowSet> {
        let expressions = procedure_argument_expressions(clause)?;
        let yield_items = resolved_yield_items(clause, procedure.outputs);
        let columns = procedure_columns(input.columns.clone(), &yield_items);
        let options = self.options.ok_or_else(|| {
            QueryError::invalid_argument(
                "Version Procedures cannot execute inside a graph-mutation clause program",
            )
        })?;
        let mutation = super::super::super::registry::is_version_mutation(name);
        let mut rows = Vec::new();
        for input_row in input.rows {
            let pinned_commit = if self.version_mutated {
                super::super::super::version::current_operation_commit(self.connection, options)?
            } else {
                self.snapshot.commit()
            };
            let args = expressions
                .iter()
                .map(|expression| self.evaluate(expression, &input_row))
                .collect::<QueryResult<Vec<_>>>()?;
            let outcome = super::super::super::version::execute_procedure(
                self.connection,
                name,
                args,
                options,
                pinned_commit,
            )?;
            if mutation {
                self.version_mutated = true;
                self.version_summary_commit = Some(outcome.summary_commit);
            }
            for procedure_row in outcome.rows {
                rows.push(self.join_procedure_row(
                    &input_row,
                    &procedure_row,
                    &yield_items,
                    name,
                )?);
            }
        }
        let rows = self.filter_yield_rows(clause, rows)?;
        Ok(RowSet { columns, rows })
    }

    fn execute_fulltext_procedure(
        &mut self,
        clause: &AstNode,
        input: RowSet,
        name: &str,
        procedure: super::super::super::registry::ProcedureDefinition,
    ) -> QueryResult<RowSet> {
        let expressions = fulltext_argument_expressions(clause, name)?;
        let relationship_query = name.eq_ignore_ascii_case("db.index.fulltext.queryRelationships");
        let yield_items = resolved_yield_items(clause, procedure.outputs);
        let columns = procedure_columns(input.columns.clone(), &yield_items);
        let mut rows = Vec::new();
        for input_row in input.rows {
            rows.extend(self.fulltext_rows_for_input(
                &input_row,
                &expressions,
                relationship_query,
                &yield_items,
                name,
            )?);
        }
        let rows = self.filter_yield_rows(clause, rows)?;
        Ok(RowSet { columns, rows })
    }

    fn fulltext_rows_for_input(
        &mut self,
        input_row: &BindingRow,
        expressions: &[expression::Expr],
        relationship_query: bool,
        yield_items: &[(String, String)],
        name: &str,
    ) -> QueryResult<Vec<BindingRow>> {
        let (index_name, query, options) =
            self.evaluate_fulltext_arguments(input_row, expressions, name)?;
        let index = resolve_semantic_index(
            self.connection,
            &self.snapshot,
            &index_name,
            StandardIndexKind::FullText,
        )?;
        let hits = fulltext_query(
            self.connection,
            &self.snapshot,
            self.graph_view,
            &index,
            &FullTextQueryInput {
                relationship_query,
                query: &query,
                skip: options.skip,
                limit: options.limit,
                analyzer: options.analyzer.as_deref(),
            },
            self.is_interrupted,
        )?;
        hits.into_iter()
            .map(|hit| {
                let procedure_row = self.fulltext_procedure_row(hit)?;
                self.join_procedure_row(input_row, &procedure_row, yield_items, name)
            })
            .collect()
    }

    fn evaluate_fulltext_arguments(
        &mut self,
        input_row: &BindingRow,
        expressions: &[expression::Expr],
        name: &str,
    ) -> QueryResult<(String, String, FullTextQueryOptions)> {
        let index_name = require_string_argument(
            self.evaluate(&expressions[0], input_row)?,
            name,
            "indexName",
        )?;
        let query = require_string_argument(
            self.evaluate(&expressions[1], input_row)?,
            name,
            "queryString",
        )?;
        let options = expressions
            .get(2)
            .map(|options| self.evaluate(options, input_row))
            .transpose()?
            .map(parse_fulltext_options)
            .transpose()?
            .unwrap_or_default();
        Ok((index_name, query, options))
    }

    fn fulltext_procedure_row(&self, hit: SemanticHit) -> QueryResult<BTreeMap<String, Value>> {
        let (field, value) = match hit.entity {
            SemanticEntity::Node(node) => (
                "node",
                Value::Node(super::super::super::graph::materialize_node(
                    &self.snapshot,
                    node,
                )?),
            ),
            SemanticEntity::Relationship(relationship) => (
                "relationship",
                Value::Relationship(super::super::super::graph::materialize_relationship(
                    &self.snapshot,
                    relationship,
                )?),
            ),
        };
        Ok(BTreeMap::from([
            (field.to_owned(), value),
            ("score".to_owned(), Value::Float(hit.score)),
        ]))
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
            rows: show_registry_rows(self.connection, &self.snapshot, clause)?,
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
