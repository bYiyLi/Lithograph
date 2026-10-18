use crate::storage::{self, OwnerKind, StandardIndexKind};

use super::super::super::managed_semantic::{self, ManagedQueryInput};
use super::super::super::semantic_index::{
    FullTextQueryInput, SemanticEntity, fulltext_query_controlled, resolve_semantic_index,
};

use super::helpers::{
    function_registry_rows, procedure_registry_rows, project_named_columns, yield_items,
};
use super::*;
use crate::query::resources::{allocation_bytes, checked_add};

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
    if let Some(expected) = semantic_argument_count(name) {
        if count == expected {
            return Ok(());
        }
        return Err(QueryError::semantic(format!(
            "procedure {name} expects {expected} arguments"
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

fn semantic_argument_count(name: &str) -> Option<usize> {
    Some(match name.to_ascii_lowercase().as_str() {
        "db.index.semantic.createnodeindex" | "db.index.semantic.createrelationshipindex" => 4,
        "db.index.semantic.querynodes" | "db.index.semantic.queryrelationships" => 3,
        "db.index.semantic.rebuild" => 2,
        _ => return None,
    })
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
        | "lithograph.index.rebuild"
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

fn procedure_projection(
    columns: Vec<String>,
    clause: &AstNode,
    outputs: &[&str],
) -> (Vec<(String, String)>, Vec<String>) {
    let yield_items = resolved_yield_items(clause, outputs);
    let columns = procedure_columns(columns, &yield_items);
    (yield_items, columns)
}

fn show_registry_rows(
    connection: &Connection,
    snapshot: &Snapshot<'_>,
    clause: &AstNode,
    control: &ExecutionControl<'_>,
) -> QueryResult<(Vec<BindingRow>, Option<MemoryReservation>)> {
    if clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::ShowTarget(crate::cypher::ShowTargetKind::Functions)
        )
    }) {
        let (rows, memory) = function_registry_rows(control)?;
        return Ok((rows, Some(memory)));
    }
    if clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::ShowTarget(crate::cypher::ShowTargetKind::Procedures)
        )
    }) {
        let (rows, memory) = procedure_registry_rows(control)?;
        return Ok((rows, Some(memory)));
    }
    Ok((
        super::super::super::schema::show_rows(connection, snapshot, clause)?,
        None,
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

#[derive(Default)]
struct FullTextQueryOptions {
    skip: usize,
    limit: Option<usize>,
    analyzer: Option<String>,
}

struct SemanticQueryOptions {
    skip: usize,
    limit: usize,
}

fn require_string_argument(value: Value, procedure: &str, argument: &str) -> QueryResult<String> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err(QueryError::semantic(format!(
            "procedure {procedure} argument {argument} must be String"
        ))),
    }
}

fn require_semantic_string(value: Value, argument: &str) -> QueryResult<String> {
    match value {
        Value::String(value) => Ok(value),
        _ => Err(QueryError::invalid_argument(format!(
            "semantic procedure argument {argument} must be STRING"
        ))),
    }
}

fn parse_fulltext_options(value: Value) -> QueryResult<FullTextQueryOptions> {
    let Value::Map(mut options) = value else {
        return Err(QueryError::semantic(
            "full-text query options must be a Map",
        ));
    };
    let skip =
        take_nonnegative_integer(&mut options, "skip", IntegerOptionFamily::FullText)?.unwrap_or(0);
    let limit = take_nonnegative_integer(&mut options, "limit", IntegerOptionFamily::FullText)?;
    let analyzer = match options.remove("analyzer") {
        None => None,
        Some(Value::String(value)) => {
            super::super::super::semantic_index::validate_fulltext_query_specification(&value)?;
            Some(value)
        }
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

fn parse_semantic_options(value: Value) -> QueryResult<SemanticQueryOptions> {
    let Value::Map(mut options) = value else {
        return Err(QueryError::invalid_argument(
            "semantic query options must be a MAP",
        ));
    };
    let skip =
        take_nonnegative_integer(&mut options, "skip", IntegerOptionFamily::Semantic)?.unwrap_or(0);
    let limit = take_nonnegative_integer(&mut options, "limit", IntegerOptionFamily::Semantic)?
        .ok_or_else(|| QueryError::invalid_argument("semantic query options require limit"))?;
    if let Some(key) = options.keys().next() {
        return Err(QueryError::invalid_argument(format!(
            "unsupported semantic query option {key:?}"
        )));
    }
    Ok(SemanticQueryOptions { skip, limit })
}

#[derive(Clone, Copy)]
enum IntegerOptionFamily {
    Semantic,
    FullText,
}

fn take_nonnegative_integer(
    options: &mut BTreeMap<String, Value>,
    key: &str,
    family: IntegerOptionFamily,
) -> QueryResult<Option<usize>> {
    let Some(value) = options.remove(key) else {
        return Ok(None);
    };
    let Value::Integer(value) = value else {
        return Err(match family {
            IntegerOptionFamily::Semantic => {
                QueryError::invalid_argument(format!("semantic query option {key} must be INTEGER"))
            }
            IntegerOptionFamily::FullText => {
                QueryError::semantic(format!("full-text option {key} must be Integer"))
            }
        });
    };
    usize::try_from(value).map(Some).map_err(|_| match family {
        IntegerOptionFamily::Semantic => {
            QueryError::invalid_argument(format!("semantic query option {key} cannot be negative"))
        }
        IntegerOptionFamily::FullText => {
            QueryError::semantic(format!("full-text option {key} cannot be negative"))
        }
    })
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

struct ProcedureSetup {
    expressions: Vec<expression::Expr>,
    yield_items: Vec<(String, String)>,
    columns: Vec<String>,
}

fn procedure_setup(
    columns: Vec<String>,
    clause: &AstNode,
    outputs: &[&str],
) -> QueryResult<ProcedureSetup> {
    let expressions = procedure_argument_expressions(clause)?;
    let (yield_items, columns) = procedure_projection(columns, clause, outputs);
    Ok(ProcedureSetup {
        expressions,
        yield_items,
        columns,
    })
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
        if super::super::super::registry::is_semantic_procedure(name) {
            return self.execute_semantic_procedure(clause, input, name, procedure);
        }
        let procedure_rows = self.current_graph_procedure(name)?;
        self.retain_procedure_rows(&procedure_rows)?;
        let (yield_items, columns) =
            procedure_projection(input.columns.clone(), clause, procedure.outputs);
        let rows = self.join_procedure_rows(input.rows, &procedure_rows, &yield_items, name)?;
        let rows = self.filter_yield_rows(clause, rows)?;
        self.registry_row_set(columns, rows)
    }

    fn procedure_arguments(
        &mut self,
        expressions: &[expression::Expr],
        row: &BindingRow,
    ) -> QueryResult<Vec<Value>> {
        self.expression_reservation
            .grow(allocation_bytes::<Value>(expressions.len())?)?;
        expressions
            .iter()
            .map(|expression| self.evaluate(expression, row))
            .collect()
    }

    fn execute_version_procedure(
        &mut self,
        clause: &AstNode,
        input: RowSet,
        name: &str,
        procedure: super::super::super::registry::ProcedureDefinition,
    ) -> QueryResult<RowSet> {
        let setup = procedure_setup(input.columns.clone(), clause, procedure.outputs)?;
        if self.options.is_none() {
            return Err(QueryError::invalid_argument(
                "Version Procedures cannot execute inside a graph-mutation clause program",
            ));
        }
        let mutation = super::super::super::registry::is_version_mutation(name);
        let mut rows = Vec::new();
        for input_row in input.rows {
            self.control.checkpoint(1)?;
            let previous = self.expression_reservation.bytes();
            let produced = self.execute_version_input(&input_row, &setup, name, mutation)?;
            self.retain_registry_output(previous, &produced)?;
            rows.extend(produced);
        }
        let rows = self.filter_yield_rows(clause, rows)?;
        self.registry_row_set(setup.columns, rows)
    }

    fn execute_version_input(
        &mut self,
        input_row: &BindingRow,
        setup: &ProcedureSetup,
        name: &str,
        mutation: bool,
    ) -> QueryResult<Vec<BindingRow>> {
        let options = self
            .options
            .ok_or_else(|| QueryError::internal("Version procedure options are missing"))?;
        let pinned_commit = if self.version_mutated {
            super::super::super::version::current_operation_commit(self.connection, options)?
        } else {
            self.snapshot.commit()
        };
        let args = self.procedure_arguments(&setup.expressions, input_row)?;
        let outcome = super::super::super::version::execute_procedure(
            self.connection,
            name,
            args,
            options,
            pinned_commit,
            self.is_interrupted,
            self.control,
        )?;
        self.control.check()?;
        self.retain_procedure_rows(&outcome.rows)?;
        if mutation {
            self.version_mutated = true;
            self.version_summary_commit = Some(outcome.summary_commit);
        }
        let mut rows = Vec::new();
        for procedure_row in outcome.rows {
            self.control.checkpoint(1)?;
            rows.push(self.join_procedure_row(
                input_row,
                &procedure_row,
                &setup.yield_items,
                name,
            )?);
        }
        Ok(rows)
    }

    fn execute_semantic_procedure(
        &mut self,
        clause: &AstNode,
        input: RowSet,
        name: &str,
        procedure: super::super::super::registry::ProcedureDefinition,
    ) -> QueryResult<RowSet> {
        let ProcedureSetup {
            expressions,
            yield_items,
            columns,
        } = procedure_setup(input.columns.clone(), clause, procedure.outputs)?;
        let rows = if super::super::super::registry::is_semantic_query(name) {
            self.semantic_query_rows(input.rows, &expressions, &yield_items, name)?
        } else {
            self.validate_semantic_maintenance_options(name)?;
            self.semantic_maintenance_rows(input.rows, &expressions, &yield_items, name)?
        };
        let rows = self.filter_yield_rows(clause, rows)?;
        self.registry_row_set(columns, rows)
    }

    fn semantic_query_rows(
        &mut self,
        input_rows: Vec<BindingRow>,
        expressions: &[expression::Expr],
        yield_items: &[(String, String)],
        name: &str,
    ) -> QueryResult<Vec<BindingRow>> {
        let relationship_query = name.eq_ignore_ascii_case("db.index.semantic.queryRelationships");
        let mut rows = Vec::new();
        for input_row in input_rows {
            self.control.checkpoint(1)?;
            let previous = self.expression_reservation.bytes();
            let start = rows.len();
            let (index_name, query_text, options) =
                self.evaluate_semantic_arguments(&input_row, expressions)?;
            let (hits, _hits_memory) =
                self.semantic_hits(&index_name, &query_text, options, relationship_query)?;
            for hit in hits {
                self.control.checkpoint(1)?;
                rows.push(self.join_semantic_hit(&input_row, hit, yield_items, name)?);
            }
            drop(_hits_memory);
            drop(index_name);
            drop(query_text);
            self.retain_registry_output(previous, &rows[start..])?;
        }
        Ok(rows)
    }

    fn evaluate_semantic_arguments(
        &mut self,
        input_row: &BindingRow,
        expressions: &[expression::Expr],
    ) -> QueryResult<(String, String, SemanticQueryOptions)> {
        let index_name =
            require_semantic_string(self.evaluate(&expressions[0], input_row)?, "indexName")?;
        let query_text =
            require_semantic_string(self.evaluate(&expressions[1], input_row)?, "queryString")?;
        let options = parse_semantic_options(self.evaluate(&expressions[2], input_row)?)?;
        Ok((index_name, query_text, options))
    }

    fn semantic_hits(
        &self,
        index_name: &str,
        query_text: &str,
        options: SemanticQueryOptions,
        relationship_query: bool,
    ) -> QueryResult<(Vec<SemanticHit>, MemoryReservation)> {
        let index = resolve_semantic_index(
            self.connection,
            &self.snapshot,
            index_name,
            StandardIndexKind::Semantic,
        )?;
        super::super::super::semantic_index::validate_query_index_access(&index, self.graph_view)?;
        managed_semantic::query_controlled(
            self.connection,
            &self.snapshot,
            self.graph_view,
            &index,
            &ManagedQueryInput {
                relationship_query,
                query: query_text,
                skip: options.skip,
                limit: options.limit,
            },
            self.is_interrupted,
            self.control.resources(),
        )
    }

    fn validate_semantic_maintenance_options(&self, name: &str) -> QueryResult<()> {
        let options = self.options.ok_or_else(|| {
            QueryError::invalid_argument(
                "Semantic maintenance cannot execute inside a graph-mutation clause program",
            )
        })?;
        if !super::super::super::registry::is_semantic_maintenance(name) {
            return Ok(());
        }
        if !options.graph_view.is_full_graph() {
            return Err(QueryError::invalid_argument(
                "Semantic maintenance does not accept options.graphView",
            ));
        }
        if !matches!(options.snapshot, crate::query::SnapshotSelector::Current) {
            return Err(QueryError::invalid_argument(
                "Semantic maintenance does not accept options.at",
            ));
        }
        Ok(())
    }

    fn semantic_maintenance_rows(
        &mut self,
        input_rows: Vec<BindingRow>,
        expressions: &[expression::Expr],
        yield_items: &[(String, String)],
        name: &str,
    ) -> QueryResult<Vec<BindingRow>> {
        let mut rows = Vec::new();
        for input_row in input_rows {
            self.control.checkpoint(1)?;
            let previous = self.expression_reservation.bytes();
            let args = self.procedure_arguments(expressions, &input_row)?;
            let procedure_row = self.semantic_maintenance_row(name, args)?;
            let row = self.join_procedure_row(&input_row, &procedure_row, yield_items, name)?;
            drop(procedure_row);
            self.retain_registry_output(previous, std::slice::from_ref(&row))?;
            rows.push(row);
        }
        Ok(rows)
    }

    fn semantic_maintenance_row(
        &mut self,
        name: &str,
        args: Vec<Value>,
    ) -> QueryResult<BTreeMap<String, Value>> {
        match name.to_ascii_lowercase().as_str() {
            "db.index.semantic.rebuild" => {
                let mut args = args.into_iter();
                let index_name =
                    require_semantic_string(args.next().unwrap_or(Value::Null), "name")?;
                let version =
                    require_semantic_string(args.next().unwrap_or(Value::Null), "version")?;
                let outcome = managed_semantic::rebuild_controlled(
                    self.connection,
                    &index_name,
                    &version,
                    self.is_interrupted,
                    self.control.resources(),
                )?;
                self.version_summary_commit = Some(outcome.commit);
                Ok(BTreeMap::from([
                    ("name".to_owned(), Value::String(index_name)),
                    (
                        "commit".to_owned(),
                        Value::String(format!("commit/{}", outcome.commit.to_hex())),
                    ),
                    (
                        "indexedEntities".to_owned(),
                        Value::Integer(i64::try_from(outcome.indexed_entities).unwrap_or(i64::MAX)),
                    ),
                    (
                        "embeddedTexts".to_owned(),
                        Value::Integer(i64::try_from(outcome.embedded_texts).unwrap_or(i64::MAX)),
                    ),
                ]))
            }
            _ => Err(QueryError::internal(format!(
                "registered Semantic procedure {name} has no executor"
            ))),
        }
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
        let (yield_items, columns) =
            procedure_projection(input.columns.clone(), clause, procedure.outputs);
        let mut rows = Vec::new();
        for input_row in input.rows {
            self.control.checkpoint(1)?;
            let previous = self.expression_reservation.bytes();
            let produced = self.fulltext_rows_for_input(
                &input_row,
                &expressions,
                relationship_query,
                &yield_items,
                name,
            )?;
            self.retain_registry_output(previous, &produced)?;
            rows.extend(produced);
        }
        let rows = self.filter_yield_rows(clause, rows)?;
        self.registry_row_set(columns, rows)
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
        super::super::super::semantic_index::validate_query_index_access(&index, self.graph_view)?;
        let result = fulltext_query_controlled(
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
            self.control,
        )?;
        let _hits_memory = result.memory;
        let mut rows = Vec::new();
        for hit in result.hits {
            self.control.checkpoint(1)?;
            rows.push(self.join_semantic_hit(input_row, hit, yield_items, name)?);
        }
        Ok(rows)
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

    fn semantic_hit_row(&mut self, hit: SemanticHit) -> QueryResult<BTreeMap<String, Value>> {
        self.control.checkpoint(1)?;
        let (field, binding) = match hit.entity {
            SemanticEntity::Node(node) => ("node", BindingValue::Node(node)),
            SemanticEntity::Relationship(relationship) => {
                ("relationship", BindingValue::Relationship(relationship))
            }
        };
        let evaluated = expression::binding_value_with_memory(
            &self.snapshot,
            &binding,
            &self.graph_view.query_access,
            self.control,
        )?;
        self.expression_reservation.merge(evaluated.reservation)?;
        self.expression_reservation.grow(512)?;
        let value = evaluated.value;
        Ok(BTreeMap::from([
            (field.to_owned(), value),
            ("score".to_owned(), Value::Float(hit.score)),
        ]))
    }

    fn join_semantic_hit(
        &mut self,
        input_row: &BindingRow,
        hit: SemanticHit,
        yield_items: &[(String, String)],
        name: &str,
    ) -> QueryResult<BindingRow> {
        let previous = self.expression_reservation.bytes();
        let procedure_row = self.semantic_hit_row(hit)?;
        let row = self.join_procedure_row(input_row, &procedure_row, yield_items, name)?;
        drop(procedure_row);
        self.retain_registry_output(previous, std::slice::from_ref(&row))?;
        Ok(row)
    }

    fn retain_registry_output(&mut self, previous: u64, rows: &[BindingRow]) -> QueryResult<()> {
        let bytes =
            rows.iter()
                .try_fold(allocation_bytes::<BindingRow>(rows.len())?, |bytes, row| {
                    self.control.checkpoint(1)?;
                    checked_add(
                        bytes,
                        expression::binding_row_bytes_controlled(row, Some(self.control))?,
                    )
                })?;
        self.expression_reservation
            .resize(checked_add(previous, bytes)?)
    }

    fn registry_row_set(
        &mut self,
        columns: Vec<String>,
        rows: Vec<BindingRow>,
    ) -> QueryResult<RowSet> {
        let memory = std::mem::replace(&mut self.expression_reservation, self.control.reserve(0)?);
        let mut result = RowSet {
            columns,
            rows,
            memory: Some(memory),
        };
        result.track(self.control)?;
        Ok(result)
    }

    fn retain_procedure_rows(&mut self, rows: &[BTreeMap<String, Value>]) -> QueryResult<()> {
        let mut bytes = allocation_bytes::<BTreeMap<String, Value>>(rows.len().saturating_mul(2))?;
        for row in rows {
            self.control.checkpoint(1)?;
            for (name, value) in row {
                self.control.checkpoint(1)?;
                bytes = checked_add(
                    bytes,
                    checked_add(
                        expression::value_bytes_with_control(value, Some(self.control))?,
                        256 + name.capacity() as u64,
                    )?,
                )?;
            }
        }
        self.expression_reservation.grow(bytes)
    }

    fn cross_join_registry(&mut self, input: RowSet, registry: RowSet) -> QueryResult<RowSet> {
        let mut columns = input.columns.clone();
        for column in &registry.columns {
            self.control.checkpoint(1)?;
            if !columns.contains(column) {
                columns.push(column.clone());
            }
        }
        let mut rows = Vec::new();
        for outer in input.rows {
            for shown in &registry.rows {
                self.control.checkpoint(1)?;
                let combined = self.join_registry_row(&outer, shown, &registry.columns)?;
                rows.push(combined);
            }
        }
        self.registry_row_set(columns, rows)
    }

    fn join_registry_row(
        &mut self,
        outer: &BindingRow,
        shown: &BindingRow,
        columns: &[String],
    ) -> QueryResult<BindingRow> {
        self.expression_reservation.grow(checked_add(
            checked_add(
                expression::binding_row_bytes_controlled(outer, Some(self.control))?,
                expression::binding_row_bytes_controlled(shown, Some(self.control))?,
            )?,
            allocation_bytes::<BindingRow>(1)?,
        )?)?;
        let mut combined =
            expression::clone_binding_row_unreserved_controlled(outer, Some(self.control))?;
        for column in columns {
            self.control.checkpoint(1)?;
            let value = shown
                .values
                .get(column)
                .map(|value| {
                    expression::clone_binding_value_unreserved_controlled(value, Some(self.control))
                })
                .transpose()?
                .unwrap_or(BindingValue::Null);
            combined.insert(column.clone(), value);
        }
        Ok(combined)
    }

    fn join_procedure_rows(
        &mut self,
        input_rows: Vec<BindingRow>,
        procedure_rows: &[BTreeMap<String, Value>],
        yield_items: &[(String, String)],
        name: &str,
    ) -> QueryResult<Vec<BindingRow>> {
        let mut rows = Vec::new();
        for input in input_rows {
            self.control.checkpoint(1)?;
            for procedure_row in procedure_rows {
                self.control.checkpoint(1)?;
                rows.push(self.join_procedure_row(&input, procedure_row, yield_items, name)?);
            }
        }
        Ok(rows)
    }

    fn join_procedure_row(
        &mut self,
        input: &BindingRow,
        procedure_row: &BTreeMap<String, Value>,
        yield_items: &[(String, String)],
        name: &str,
    ) -> QueryResult<BindingRow> {
        self.reserve_procedure_row(input, procedure_row, yield_items)?;
        let mut row =
            expression::clone_binding_row_unreserved_controlled(input, Some(self.control))?;
        for (source, output) in yield_items {
            self.control.checkpoint(1)?;
            let value = procedure_row.get(source).ok_or_else(|| {
                QueryError::semantic(format!(
                    "procedure {name} does not yield output field {source:?}"
                ))
            })?;
            let value = expression::clone_value_unreserved_controlled(value, Some(self.control))?;
            row.insert(
                output.clone(),
                binding_from_value_controlled(&self.snapshot, value, self.control)?,
            );
        }
        Ok(row)
    }

    fn reserve_procedure_row(
        &mut self,
        input: &BindingRow,
        procedure_row: &BTreeMap<String, Value>,
        yield_items: &[(String, String)],
    ) -> QueryResult<()> {
        let mut bytes = checked_add(
            expression::binding_row_bytes_controlled(input, Some(self.control))?,
            allocation_bytes::<BindingRow>(1)?,
        )?;
        for (source, output) in yield_items {
            self.control.checkpoint(1)?;
            if let Some(value) = procedure_row.get(source) {
                bytes = checked_add(
                    bytes,
                    checked_add(
                        expression::value_bytes_with_control(value, Some(self.control))?,
                        256 + output.len() as u64,
                    )?,
                )?;
            }
        }
        self.expression_reservation.grow(bytes)?;
        Ok(())
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
        match name.to_ascii_lowercase().as_str() {
            "db.labels" => self.current_labels(),
            "db.relationshiptypes" => self.current_relationship_types(),
            "db.propertykeys" => self.current_property_keys(),
            _ => Err(QueryError::internal(format!(
                "registered procedure {name} has no executor"
            ))),
        }
    }

    fn current_labels(&self) -> QueryResult<Vec<BTreeMap<String, Value>>> {
        let mut values = BTreeSet::new();
        let mut memory = self.control.reserve(0)?;
        self.visit_visible_nodes(|node| {
            for label in self.snapshot.labels(node)? {
                self.control.checkpoint(1)?;
                if let Some(name) = storage::label_name(self.connection, label)? {
                    insert_registry_name(&mut values, &mut memory, name, self.control)?;
                }
            }
            Ok(())
        })?;
        single_column_registry_rows("label", values, &mut memory, self.control)
    }

    fn current_relationship_types(&self) -> QueryResult<Vec<BTreeMap<String, Value>>> {
        let mut values = BTreeSet::new();
        let mut memory = self.control.reserve(0)?;
        self.visit_visible_relationships(|relationship| {
            if let Some(name) =
                storage::relationship_type_name(self.connection, relationship.type_id)?
            {
                insert_registry_name(&mut values, &mut memory, name, self.control)?;
            }
            Ok(())
        })?;
        single_column_registry_rows("relationshipType", values, &mut memory, self.control)
    }

    fn current_property_keys(&self) -> QueryResult<Vec<BTreeMap<String, Value>>> {
        let mut values = BTreeSet::new();
        let mut memory = self.control.reserve(0)?;
        self.collect_node_property_keys(&mut values, &mut memory)?;
        self.collect_relationship_property_keys(&mut values, &mut memory)?;
        single_column_registry_rows("propertyKey", values, &mut memory, self.control)
    }

    fn collect_node_property_keys(
        &self,
        values: &mut BTreeSet<String>,
        memory: &mut MemoryReservation,
    ) -> QueryResult<()> {
        self.visit_visible_nodes(|node| {
            self.collect_owner_property_keys(OwnerKind::Node, node, values, memory)
        })
    }

    fn visit_visible_nodes(
        &self,
        mut visit: impl FnMut(i64) -> QueryResult<()>,
    ) -> QueryResult<()> {
        let mut after = 0_i64;
        loop {
            self.control.check()?;
            let _page_memory = self.control.reserve(allocation_bytes::<i64>(1024)?)?;
            let page = self.snapshot.scan_nodes_after(after, 256)?;
            for node in page.items {
                self.control.checkpoint(1)?;
                if self
                    .graph_view
                    .visible_node_controlled(&self.snapshot, node, self.control)?
                {
                    visit(node)?;
                }
            }
            match page.next_after {
                Some(next_after) => after = next_after,
                None => return Ok(()),
            }
        }
    }

    fn collect_relationship_property_keys(
        &self,
        values: &mut BTreeSet<String>,
        memory: &mut MemoryReservation,
    ) -> QueryResult<()> {
        self.visit_visible_relationships(|relationship| {
            self.collect_owner_property_keys(
                OwnerKind::Relationship,
                relationship.id,
                values,
                memory,
            )
        })
    }

    fn visit_visible_relationships(
        &self,
        mut visit: impl FnMut(crate::storage::RelationshipRecord) -> QueryResult<()>,
    ) -> QueryResult<()> {
        let mut after = 0_i64;
        loop {
            self.control.check()?;
            let _page_memory = self.control.reserve(allocation_bytes::<
                crate::storage::RelationshipRecord,
            >(1024)?)?;
            let page = self.snapshot.scan_relationships_after(after, 256)?;
            for relationship in page.items {
                self.control.checkpoint(1)?;
                if self.graph_view.visible_relationship_controlled(
                    &self.snapshot,
                    relationship,
                    self.control,
                )? {
                    visit(relationship)?;
                }
            }
            match page.next_after {
                Some(next_after) => after = next_after,
                None => return Ok(()),
            }
        }
    }

    fn collect_owner_property_keys(
        &self,
        owner_kind: OwnerKind,
        owner_id: i64,
        values: &mut BTreeSet<String>,
        memory: &mut MemoryReservation,
    ) -> QueryResult<()> {
        for (key, _) in self.snapshot.properties(owner_kind, owner_id)? {
            self.control.checkpoint(1)?;
            if let Some(name) = storage::property_key_name(self.connection, key)?
                && !self.graph_view.query_access.denies(&name)
            {
                insert_registry_name(values, memory, name, self.control)?;
            }
        }
        Ok(())
    }

    pub(super) fn execute_show(&mut self, clause: &AstNode, input: RowSet) -> QueryResult<RowSet> {
        let all_columns = super::super::show_all_columns(clause)?;
        let default_columns = super::super::show_default_columns(clause)?;
        let yield_node = crate::cypher::show_yield_node(clause);
        let (rows, memory) =
            show_registry_rows(self.connection, &self.snapshot, clause, self.control)?;
        let mut registry = RowSet {
            columns: all_columns.clone(),
            rows,
            memory,
        };
        registry.track(self.control)?;
        if let Some(yield_node) = yield_node {
            registry = self.execute_projection(
                &crate::cypher::show_projection_clause(yield_node),
                registry,
                false,
            )?;
            if yield_node
                .descendants()
                .any(|node| node.kind == AstKind::YieldAll)
            {
                registry.columns = all_columns;
            }
        }
        let input_columns = input.columns.clone();
        registry.track(self.control)?;
        let mut result = self.cross_join_registry(input, registry)?;
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
            result.track(self.control)?;
        }
        result.track(self.control)?;
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

fn insert_registry_name(
    values: &mut BTreeSet<String>,
    memory: &mut MemoryReservation,
    name: String,
    control: &ExecutionControl<'_>,
) -> QueryResult<()> {
    control.checkpoint(1)?;
    if !values.contains(&name) {
        memory.grow(checked_add(name.capacity() as u64, 128)?)?;
        values.insert(name);
    }
    Ok(())
}

fn single_column_registry_rows(
    column: &str,
    values: BTreeSet<String>,
    memory: &mut MemoryReservation,
    control: &ExecutionControl<'_>,
) -> QueryResult<Vec<BTreeMap<String, Value>>> {
    memory.grow(crate::query::resources::checked_mul(
        values.len() as u64,
        256 + column.len() as u64 + std::mem::size_of::<BTreeMap<String, Value>>() as u64,
    )?)?;
    let mut rows = Vec::with_capacity(values.len());
    for value in values {
        control.checkpoint(1)?;
        rows.push(BTreeMap::from([(column.to_owned(), Value::String(value))]));
    }
    Ok(rows)
}
