use std::collections::BTreeMap;

use rusqlite::Connection;

use crate::cypher::{
    AstKind, AstNode, ClauseKind, ExecutionMode, QueryAst, QueryConnector, ShowTargetKind, Value,
};
use crate::storage::HashId;

use super::expression::{BindingRow, BindingValue, compile_expression};
use super::graph::ResolvedGraphView;
use super::options::{ExecutionOptions, GraphViewSelector, writable_branch};
use super::{
    LogicalOperator, LogicalPlan, PhysicalOperator, PhysicalPlan, QueryError, QueryMetrics,
    QueryResult,
};

pub(crate) mod execute;
mod path;

use execute::execute_read;

#[derive(Debug, Clone)]
pub(crate) struct PreparedProgram {
    pub(crate) root: AstNode,
    pub(crate) source: String,
    pub(crate) columns: Vec<String>,
    pub(crate) writes: bool,
    pub(crate) logical: LogicalPlan,
    pub(crate) physical: PhysicalPlan,
    pub(crate) write_options: Option<ProgramWriteOptions>,
    pub(crate) transaction_options: Option<TransactionProgramOptions>,
}

#[derive(Debug, Clone)]
pub(crate) struct ProgramWriteOptions {
    pub(crate) graph_view: GraphViewSelector,
    pub(crate) branch: String,
    pub(crate) author: Option<String>,
    pub(crate) message: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) struct TransactionProgramOptions {
    pub(crate) graph_view: GraphViewSelector,
    pub(crate) branch: String,
    pub(crate) author: Option<String>,
    pub(crate) message: Option<String>,
}

pub(super) struct ComposedQueryParts<'a> {
    pub(super) first: &'a AstNode,
    pub(super) rest: Vec<(QueryConnector, &'a AstNode)>,
}

pub(super) fn composed_query_parts(query: &AstNode) -> QueryResult<ComposedQueryParts<'_>> {
    let operands = query
        .children
        .iter()
        .filter(|node| matches!(node.kind, AstKind::SingleQuery | AstKind::Subquery(_)))
        .collect::<Vec<_>>();
    let first = operands
        .first()
        .copied()
        .ok_or_else(|| QueryError::semantic("composed query has no operand"))?;
    let operand_count = operands.len();
    let connectors = query.children.iter().filter_map(|node| match node.kind {
        AstKind::Connector(connector) => Some(connector),
        _ => None,
    });
    let rest = connectors
        .zip(operands.into_iter().skip(1))
        .collect::<Vec<_>>();
    if rest.len().saturating_add(1) != operand_count {
        return Err(QueryError::internal(
            "composed query connector/operand cardinality is inconsistent",
        ));
    }
    Ok(ComposedQueryParts { first, rest })
}

pub(super) fn executable_query(body: &AstNode, include_single: bool) -> QueryResult<&AstNode> {
    body.children
        .iter()
        .find(|node| executable_query_kind(node, include_single))
        .or_else(|| executable_query_kind(body, include_single).then_some(body))
        .ok_or_else(|| QueryError::semantic("query body has no executable query"))
}

fn executable_query_kind(node: &AstNode, include_single: bool) -> bool {
    matches!(
        node.kind,
        AstKind::ComposedQuery | AstKind::ConditionalQuery
    ) || include_single && node.kind == AstKind::SingleQuery
}

pub(super) fn required_query_body<'a>(
    node: &'a AstNode,
    message: &str,
) -> QueryResult<&'a AstNode> {
    node.children
        .iter()
        .find(|child| child.kind == AstKind::QueryBody)
        .ok_or_else(|| QueryError::semantic(message))
}

pub(super) fn explicit_imports(scope: Option<&AstNode>, outer: &[String]) -> Vec<String> {
    let Some(scope) = scope else {
        return Vec::new();
    };
    if scope
        .descendants()
        .any(|node| node.kind == AstKind::SubqueryScopeAll)
    {
        outer.to_vec()
    } else {
        scope
            .descendants()
            .filter(|node| node.kind == AstKind::SubqueryImport)
            .filter_map(|node| node.text.clone())
            .collect()
    }
}

pub(super) fn project_bindings(row: &BindingRow, names: &[String]) -> BindingRow {
    let mut projected = BindingRow::default();
    for name in names {
        projected.insert(
            name.clone(),
            row.values.get(name).cloned().unwrap_or(BindingValue::Null),
        );
    }
    projected.load_csv_context.clone_from(&row.load_csv_context);
    projected
}

pub(crate) fn requires_program(root: &AstNode) -> bool {
    root.descendants().any(|node| match node.kind {
        AstKind::ConditionalQuery | AstKind::Connector(_) | AstKind::Search => true,
        AstKind::Subquery(
            crate::cypher::SubqueryKind::Exists
            | crate::cypher::SubqueryKind::Count
            | crate::cypher::SubqueryKind::Collect,
        ) => true,
        AstKind::Clause(
            ClauseKind::With
            | ClauseKind::Let
            | ClauseKind::Unwind
            | ClauseKind::For
            | ClauseKind::Filter
            | ClauseKind::Call
            | ClauseKind::LoadCsv
            | ClauseKind::Show
            | ClauseKind::Foreach,
        ) => true,
        AstKind::GroupBy
        | AstKind::StarProjection
        | AstKind::SetQuantifier(_)
        | AstKind::PathMode(_)
        | AstKind::PathSelector(_)
        | AstKind::Quantifier(_)
        | AstKind::VariableLength => true,
        AstKind::Expression(kind) => !matches!(
            kind,
            crate::cypher::ExpressionKind::Expression
                | crate::cypher::ExpressionKind::Or
                | crate::cypher::ExpressionKind::Xor
                | crate::cypher::ExpressionKind::And
                | crate::cypher::ExpressionKind::Not
                | crate::cypher::ExpressionKind::Comparison
                | crate::cypher::ExpressionKind::Additive
                | crate::cypher::ExpressionKind::Multiplicative
                | crate::cypher::ExpressionKind::Power
                | crate::cypher::ExpressionKind::Unary
                | crate::cypher::ExpressionKind::Postfix
                | crate::cypher::ExpressionKind::FunctionCall
                | crate::cypher::ExpressionKind::List
                | crate::cypher::ExpressionKind::Map
                | crate::cypher::ExpressionKind::Parenthesized
                | crate::cypher::ExpressionKind::Primary
        ),
        _ => false,
    }) || has_aggregating_function(root)
        || has_advanced_pattern(root)
}

fn has_advanced_pattern(root: &AstNode) -> bool {
    root.descendants().any(|node| match node.kind {
        AstKind::MatchMode(_) | AstKind::QuantifiedPattern => true,
        AstKind::PatternPart => {
            node.descendants()
                .filter(|child| child.kind == AstKind::RelationshipPattern)
                .count()
                > 1
        }
        AstKind::NodePattern | AstKind::RelationshipPattern => {
            node.descendants().any(|child| match child.kind {
                AstKind::Where | AstKind::MapEntry => true,
                AstKind::NameExpression(
                    crate::cypher::NameExpressionKind::Dynamic
                    | crate::cypher::NameExpressionKind::Wildcard
                    | crate::cypher::NameExpressionKind::Negation(1..),
                ) => true,
                AstKind::NameExpression(
                    crate::cypher::NameExpressionKind::Disjunction
                    | crate::cypher::NameExpressionKind::Conjunction,
                ) => {
                    child
                        .children
                        .iter()
                        .filter(|nested| matches!(nested.kind, AstKind::NameExpression(_)))
                        .count()
                        > 1
                }
                _ => false,
            })
        }
        _ => false,
    })
}

fn has_aggregating_function(root: &AstNode) -> bool {
    root.descendants()
        .filter(|node| node.kind == AstKind::FunctionName)
        .filter_map(|node| node.text.as_deref())
        .any(crate::cypher::is_aggregating_function)
}

pub(crate) fn prepare_program(
    ast: &QueryAst,
    source: &str,
    options: &ExecutionOptions,
) -> QueryResult<PreparedProgram> {
    let transaction_owning = ast
        .root
        .descendants()
        .any(|node| node.kind == AstKind::TransactionSubclause);
    if transaction_owning {
        validate_transaction_program(&ast.root)?;
    }
    validate_surface_expressions(&ast.root)?;
    let columns = output_columns(&ast.root, source)?;
    let writes = contains_mutation(&ast.root);
    let write_options = writes.then(|| program_write_options(options)).transpose()?;
    let transaction_options = transaction_owning
        .then(|| transaction_program_options(options))
        .transpose()?;
    let logical_operators = logical_operators(&ast.root);
    let physical_operators = logical_operators
        .iter()
        .map(physical_operator)
        .collect::<Vec<_>>();
    Ok(PreparedProgram {
        root: ast.root.clone(),
        source: source.to_owned(),
        columns,
        writes,
        logical: LogicalPlan {
            operators: logical_operators,
        },
        physical: PhysicalPlan {
            operators: physical_operators,
        },
        write_options,
        transaction_options,
    })
}

fn transaction_program_options(
    options: &ExecutionOptions,
) -> QueryResult<TransactionProgramOptions> {
    Ok(TransactionProgramOptions {
        graph_view: options.graph_view.clone(),
        branch: writable_branch(options)?,
        author: options.author.clone(),
        message: options.message.clone(),
    })
}

fn validate_transaction_program(root: &AstNode) -> QueryResult<()> {
    for composed in root
        .descendants()
        .filter(|node| node.kind == AstKind::ComposedQuery)
    {
        let has_union = composed.children.iter().any(|child| {
            matches!(
                child.kind,
                AstKind::Connector(
                    QueryConnector::Union
                        | QueryConnector::UnionAll
                        | QueryConnector::UnionDistinct
                )
            )
        });
        if has_union
            && composed
                .descendants()
                .any(|node| node.kind == AstKind::TransactionSubclause)
        {
            return Err(QueryError::semantic(
                "CALL subqueries IN TRANSACTIONS are not supported inside UNION",
            ));
        }
    }

    for subquery in root.descendants().filter(|node| {
        node.kind == AstKind::Subquery(crate::cypher::SubqueryKind::Call)
            && node
                .children
                .iter()
                .any(|child| child.kind == AstKind::TransactionSubclause)
    }) {
        if let Some(body) = subquery
            .children
            .iter()
            .find(|child| child.kind == AstKind::QueryBody)
            && body
                .descendants()
                .any(|node| node.kind == AstKind::TransactionSubclause)
        {
            return Err(QueryError::semantic(
                "nested CALL subqueries IN TRANSACTIONS are not supported",
            ));
        }
    }

    for single in root
        .descendants()
        .filter(|node| node.kind == AstKind::SingleQuery)
    {
        let mut saw_write = false;
        for clause in single
            .children
            .iter()
            .filter(|node| matches!(node.kind, AstKind::Clause(_)))
        {
            if clause.kind == AstKind::Clause(ClauseKind::Call)
                && clause
                    .descendants()
                    .any(|node| node.kind == AstKind::TransactionSubclause)
                && saw_write
            {
                return Err(QueryError::semantic(
                    "CALL subqueries IN TRANSACTIONS cannot follow an outer write clause",
                ));
            }
            saw_write |= matches!(
                clause.kind,
                AstKind::Clause(
                    ClauseKind::Create
                        | ClauseKind::Insert
                        | ClauseKind::Merge
                        | ClauseKind::Set
                        | ClauseKind::Remove
                        | ClauseKind::Delete
                        | ClauseKind::DetachDelete
                        | ClauseKind::Foreach
                )
            );
        }
    }
    Ok(())
}

fn program_write_options(options: &ExecutionOptions) -> QueryResult<ProgramWriteOptions> {
    Ok(ProgramWriteOptions {
        graph_view: options.graph_view.clone(),
        branch: writable_branch(options)?,
        author: options.author.clone(),
        message: options.message.clone(),
    })
}

fn validate_surface_expressions(root: &AstNode) -> QueryResult<()> {
    for expression in root.descendants().filter(|node| {
        matches!(
            node.kind,
            AstKind::Expression(crate::cypher::ExpressionKind::Expression)
        )
    }) {
        compile_expression(expression)?;
    }
    Ok(())
}

pub(crate) fn contains_mutation(root: &AstNode) -> bool {
    root.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::Clause(
                ClauseKind::Create
                    | ClauseKind::Insert
                    | ClauseKind::Merge
                    | ClauseKind::Set
                    | ClauseKind::Remove
                    | ClauseKind::Delete
                    | ClauseKind::DetachDelete
                    | ClauseKind::Foreach
            )
        )
    })
}

fn output_columns(root: &AstNode, source: &str) -> QueryResult<Vec<String>> {
    infer_columns(root, &[], source)
}

fn projection_columns(clause: &AstNode, source: &str) -> QueryResult<Vec<String>> {
    let body = projection_body(clause)?;
    direct_projection_items(body)
        .into_iter()
        .map(|item| projection_column(item, source))
        .collect()
}

pub(super) fn infer_clause_columns(
    clause: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    infer_columns(
        &AstNode {
            kind: AstKind::SingleQuery,
            span: clause.span,
            text: None,
            children: vec![clause.clone()],
        },
        input,
        source,
    )
}

pub(super) fn infer_columns(
    node: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    match node.kind {
        AstKind::QueryBody => {
            let query = node
                .children
                .iter()
                .find(|child| {
                    matches!(
                        child.kind,
                        AstKind::ComposedQuery | AstKind::ConditionalQuery
                    )
                })
                .ok_or_else(|| QueryError::semantic("query body has no executable query"))?;
            infer_columns(query, input, source)
        }
        AstKind::ComposedQuery => infer_composed_columns(node, input, source),
        AstKind::ConditionalQuery => infer_conditional_columns(node, input, source),
        AstKind::SingleQuery => infer_single_columns(node, input, source),
        AstKind::Subquery(_) => {
            let body = node
                .children
                .iter()
                .find(|child| child.kind == AstKind::QueryBody)
                .ok_or_else(|| QueryError::semantic("braced query is missing its body"))?;
            infer_columns(body, input, source)
        }
        _ => Err(QueryError::internal(
            "column inference received a non-query AST node",
        )),
    }
}

fn infer_composed_columns(
    query: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    let parts = composed_query_parts(query)?;
    let mut columns = infer_columns(parts.first, input, source)?;
    let mut segment_input = input.to_vec();
    let mut previous_operand = parts.first;
    for (connector, operand) in parts.rest {
        if connector == crate::cypher::QueryConnector::Next {
            segment_input = if crate::cypher::query_body_returns_columns(previous_operand) {
                columns.clone()
            } else {
                Vec::new()
            };
        }
        let right = infer_columns(operand, &segment_input, source)?;
        if connector != crate::cypher::QueryConnector::Next && columns != right {
            return Err(QueryError::semantic(format!(
                "UNION column mismatch: left {columns:?}, right {right:?}"
            )));
        }
        if connector == crate::cypher::QueryConnector::Next {
            columns = right;
        }
        previous_operand = operand;
    }
    Ok(columns)
}

fn infer_conditional_columns(
    query: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    let mut columns = None;
    for branch in query
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::ConditionalBranch(_)))
    {
        let nested = branch
            .children
            .iter()
            .find(|child| matches!(child.kind, AstKind::Subquery(_)))
            .and_then(|subquery| {
                subquery
                    .children
                    .iter()
                    .find(|child| child.kind == AstKind::QueryBody)
            })
            .or_else(|| {
                branch
                    .children
                    .iter()
                    .find(|child| child.kind == AstKind::QueryBody)
            })
            .or_else(|| {
                branch
                    .children
                    .iter()
                    .find(|child| child.kind == AstKind::ComposedQuery)
            })
            .ok_or_else(|| QueryError::semantic("conditional branch is missing its query"))?;
        let branch_columns = infer_columns(nested, input, source)?;
        if let Some(expected) = &columns
            && expected != &branch_columns
        {
            return Err(QueryError::semantic(format!(
                "WHEN branch column mismatch: expected {expected:?}, found {branch_columns:?}"
            )));
        }
        columns = Some(branch_columns);
    }
    columns.ok_or_else(|| QueryError::semantic("conditional query has no branch"))
}

fn infer_single_columns(
    single: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    let mut columns = input.to_vec();
    for clause in single
        .children
        .iter()
        .filter(|child| matches!(child.kind, AstKind::Clause(_)))
    {
        let AstKind::Clause(kind) = clause.kind else {
            unreachable!();
        };
        match kind {
            ClauseKind::Match
            | ClauseKind::OptionalMatch
            | ClauseKind::Create
            | ClauseKind::Insert
            | ClauseKind::Merge => append_pattern_columns(&mut columns, clause),
            ClauseKind::Let | ClauseKind::Unwind | ClauseKind::For => {
                append_named_kind(&mut columns, clause, AstKind::BindingVariable);
            }
            ClauseKind::LoadCsv => {
                append_named_kind(&mut columns, clause, AstKind::LoadCsvBinding);
            }
            ClauseKind::With | ClauseKind::Return => {
                columns = infer_projection_columns(clause, &columns, source)?;
            }
            ClauseKind::Call => append_call_columns(&mut columns, clause, source)?,
            ClauseKind::Show => columns = infer_show_columns(clause, &columns, source)?,
            ClauseKind::Finish => columns.clear(),
            ClauseKind::Filter
            | ClauseKind::Set
            | ClauseKind::Remove
            | ClauseKind::Delete
            | ClauseKind::DetachDelete
            | ClauseKind::Foreach
            | ClauseKind::CreateIndex
            | ClauseKind::DropIndex
            | ClauseKind::CreateConstraint
            | ClauseKind::DropConstraint
            | ClauseKind::GraphType => {}
        }
    }
    Ok(columns)
}

fn infer_projection_columns(
    clause: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    let body = projection_body(clause)?;
    let mut columns = if has_surface_star(body) {
        input.to_vec()
    } else {
        Vec::new()
    };
    for column in projection_columns(clause, source)? {
        append_unique(&mut columns, column);
    }
    Ok(columns)
}

fn append_pattern_columns(columns: &mut Vec<String>, clause: &AstNode) {
    append_descendant_names(columns, clause, |kind| {
        matches!(
            kind,
            &AstKind::PatternVariable | &AstKind::RelationshipVariable
        )
    });
}

fn append_named_kind(columns: &mut Vec<String>, clause: &AstNode, kind: AstKind) {
    append_descendant_names(columns, clause, |candidate| candidate == &kind);
}

fn append_descendant_names(
    columns: &mut Vec<String>,
    clause: &AstNode,
    matches_kind: impl Fn(&AstKind) -> bool,
) {
    let mut names = clause
        .descendants()
        .filter(|node| matches_kind(&node.kind))
        .filter_map(|node| {
            node.text
                .as_ref()
                .map(|name| (node.span.start, name.clone()))
        })
        .collect::<Vec<_>>();
    names.sort_by_key(|(position, _)| *position);
    for (_, name) in names {
        append_unique(columns, name);
    }
}

fn projection_body(clause: &AstNode) -> QueryResult<&AstNode> {
    clause
        .descendants()
        .find(|node| node.kind == AstKind::ProjectionBody)
        .ok_or_else(|| QueryError::semantic("projection clause is missing its body"))
}

fn append_call_columns(
    columns: &mut Vec<String>,
    clause: &AstNode,
    source: &str,
) -> QueryResult<()> {
    if let Some(subquery) = clause.children.iter().find(|node| {
        matches!(
            node.kind,
            AstKind::Subquery(crate::cypher::SubqueryKind::Call)
        )
    }) {
        let body = subquery
            .children
            .iter()
            .find(|node| node.kind == AstKind::QueryBody)
            .ok_or_else(|| QueryError::semantic("CALL subquery is missing its query body"))?;
        if body
            .descendants()
            .any(|node| node.kind == AstKind::Clause(ClauseKind::Return))
        {
            for column in infer_columns(body, columns, source)? {
                append_unique(columns, column);
            }
        }
        return Ok(());
    }
    append_procedure_columns(columns, clause)
}

fn append_procedure_columns(columns: &mut Vec<String>, clause: &AstNode) -> QueryResult<()> {
    let name = clause
        .descendants()
        .find(|node| node.kind == AstKind::FunctionName)
        .and_then(|node| node.text.as_deref())
        .ok_or_else(|| QueryError::semantic("CALL is missing its procedure name"))?;
    let procedure = super::registry::procedure(name)
        .ok_or_else(|| QueryError::semantic(format!("unknown procedure {name}")))?;
    let yielded = procedure_yield_columns(clause, name, procedure.outputs)?;
    for output in yielded {
        append_unique(columns, output);
    }
    Ok(())
}

fn procedure_yield_columns(
    clause: &AstNode,
    name: &str,
    outputs: &[&str],
) -> QueryResult<Vec<String>> {
    let yielded = clause
        .descendants()
        .filter(|node| node.kind == AstKind::YieldItem)
        .map(|item| {
            let source = item
                .descendants()
                .find(|node| node.kind == AstKind::YieldName)
                .and_then(|node| node.text.clone())
                .ok_or_else(|| QueryError::semantic("YIELD item is missing its source field"))?;
            if !outputs.contains(&source.as_str()) {
                return Err(QueryError::semantic(format!(
                    "procedure {name} does not yield output field {source:?}"
                )));
            }
            let output = item
                .descendants()
                .find(|node| node.kind == AstKind::ProjectionAlias)
                .and_then(|node| node.text.clone())
                .unwrap_or_else(|| source.clone());
            Ok(output)
        })
        .collect::<QueryResult<Vec<_>>>()?;
    Ok(if yielded.is_empty() {
        outputs.iter().map(|name| (*name).to_owned()).collect()
    } else {
        yielded
    })
}

fn infer_show_columns(
    clause: &AstNode,
    input: &[String],
    source: &str,
) -> QueryResult<Vec<String>> {
    let items = direct_show_projection_items(clause);
    let base = if !items.is_empty() {
        items
            .iter()
            .map(|item| projection_column(item, source))
            .collect::<QueryResult<Vec<_>>>()?
    } else if clause
        .descendants()
        .any(|node| node.kind == AstKind::YieldAll)
    {
        show_all_columns(clause)?
    } else {
        show_default_columns(clause)?
    };
    let mut visible = input.to_vec();
    for column in base {
        append_unique(&mut visible, column);
    }
    if let Some(return_clause) = clause
        .children
        .iter()
        .find(|node| node.kind == AstKind::Clause(ClauseKind::Return))
    {
        infer_projection_columns(return_clause, &visible, source)
    } else {
        Ok(visible)
    }
}

fn append_unique(columns: &mut Vec<String>, name: String) {
    if !columns.contains(&name) {
        columns.push(name);
    }
}

fn projection_column(item: &AstNode, source: &str) -> QueryResult<String> {
    if let Some(alias) = item
        .descendants()
        .find(|node| node.kind == AstKind::ProjectionAlias)
        .and_then(|node| node.text.clone())
    {
        return Ok(alias);
    }
    let expression = surface_expressions(item)
        .into_iter()
        .next()
        .ok_or_else(|| QueryError::semantic("projection item is missing its expression"))?;
    source
        .get(expression.span.start..expression.span.end)
        .map(str::trim)
        .filter(|text| !text.is_empty())
        .map(str::to_owned)
        .ok_or_else(|| QueryError::semantic("projection item is missing output text"))
}

fn direct_projection_items(body: &AstNode) -> Vec<&AstNode> {
    let mut items = Vec::new();
    collect_projection_items(body, &mut items);
    items.sort_by_key(|node| node.span.start);
    items
}

pub(super) fn direct_show_projection_items(clause: &AstNode) -> Vec<&AstNode> {
    let nested_return_start = clause
        .children
        .iter()
        .find(|node| node.kind == AstKind::Clause(ClauseKind::Return))
        .map_or(usize::MAX, |node| node.span.start);
    let mut items = clause
        .descendants()
        .filter(|node| {
            node.kind == AstKind::ProjectionItem && node.span.start < nested_return_start
        })
        .collect::<Vec<_>>();
    items.sort_by_key(|node| node.span.start);
    items
}

fn collect_projection_items<'a>(node: &'a AstNode, output: &mut Vec<&'a AstNode>) {
    if matches!(node.kind, AstKind::Subquery(_)) {
        return;
    }
    if node.kind == AstKind::ProjectionItem {
        if !has_surface_star(node) {
            output.push(node);
        }
        return;
    }
    for child in &node.children {
        collect_projection_items(child, output);
    }
}

pub(super) fn has_surface_star(node: &AstNode) -> bool {
    if matches!(node.kind, AstKind::Subquery(_)) {
        return false;
    }
    node.kind == AstKind::StarProjection || node.children.iter().any(has_surface_star)
}

pub(crate) fn importing_with_columns(single: &AstNode, outer: &[String]) -> Vec<String> {
    let Some(first) = single
        .children
        .iter()
        .find(|child| matches!(child.kind, AstKind::Clause(_)))
    else {
        return Vec::new();
    };
    if first.kind != AstKind::Clause(ClauseKind::With) {
        return Vec::new();
    }
    let Some(body) = first
        .descendants()
        .find(|node| node.kind == AstKind::ProjectionBody)
    else {
        return Vec::new();
    };
    if has_surface_star(body) {
        return outer.to_vec();
    }
    let mut referenced = Vec::new();
    collect_surface_variables(body, outer, &mut referenced);
    referenced
}

fn collect_surface_variables(node: &AstNode, outer: &[String], output: &mut Vec<String>) {
    if matches!(node.kind, AstKind::Subquery(_)) {
        return;
    }
    if node.kind == AstKind::Variable
        && let Some(name) = node.text.as_ref()
        && outer.contains(name)
        && !output.contains(name)
    {
        output.push(name.clone());
    }
    for child in &node.children {
        collect_surface_variables(child, outer, output);
    }
}

pub(super) fn surface_expressions(node: &AstNode) -> Vec<&AstNode> {
    super::expression::surface_expressions(node)
}

pub(super) fn show_default_columns(clause: &AstNode) -> QueryResult<Vec<String>> {
    show_columns(clause, false)
}

pub(super) fn show_all_columns(clause: &AstNode) -> QueryResult<Vec<String>> {
    show_columns(clause, true)
}

fn show_columns(clause: &AstNode, all: bool) -> QueryResult<Vec<String>> {
    let target = clause.descendants().find_map(|node| match node.kind {
        AstKind::ShowTarget(target) => Some(target),
        _ => None,
    });
    let as_graph = clause
        .descendants()
        .any(|node| node.kind == AstKind::ShowAsGraph);
    if let Some(columns) = schema_show_columns(target, all, as_graph) {
        return Ok(columns.iter().map(|column| (*column).to_owned()).collect());
    }
    let columns: &[&str] = match (target, all) {
        (Some(ShowTargetKind::Functions), false) => &["name", "category", "description"],
        (Some(ShowTargetKind::Procedures), false) => {
            &["name", "description", "mode", "worksOnSystem"]
        }
        (Some(ShowTargetKind::Functions), true) => &[
            "name",
            "category",
            "description",
            "signature",
            "isBuiltIn",
            "argumentDescription",
            "returnDescription",
            "aggregating",
            "rolesExecution",
            "rolesBoostedExecution",
            "isDeprecated",
            "deprecatedBy",
        ],
        (Some(ShowTargetKind::Procedures), true) => &[
            "name",
            "description",
            "mode",
            "worksOnSystem",
            "signature",
            "argumentDescription",
            "returnDescription",
            "admin",
            "rolesExecution",
            "rolesBoostedExecution",
            "isDeprecated",
            "deprecatedBy",
            "option",
        ],
        _ => {
            return Err(QueryError::semantic(
                "this SHOW surface belongs to a later owning Phase",
            ));
        }
    };
    Ok(columns.iter().map(|column| (*column).to_owned()).collect())
}

fn schema_show_columns(
    target: Option<ShowTargetKind>,
    all: bool,
    as_graph: bool,
) -> Option<&'static [&'static str]> {
    match (target, all, as_graph) {
        (Some(ShowTargetKind::CurrentGraphType), _, true) => Some(&["nodes", "relationships"]),
        (Some(ShowTargetKind::CurrentGraphType), _, false) => Some(&["specification"]),
        (Some(ShowTargetKind::Indexes), false, _) => Some(&[
            "id",
            "name",
            "state",
            "populationPercent",
            "type",
            "entityType",
            "labelsOrTypes",
            "properties",
            "indexProvider",
            "owningConstraint",
            "lastRead",
            "readCount",
        ]),
        (Some(ShowTargetKind::Indexes), true, _) => Some(&[
            "id",
            "name",
            "state",
            "populationPercent",
            "type",
            "entityType",
            "labelsOrTypes",
            "properties",
            "indexProvider",
            "owningConstraint",
            "lastRead",
            "readCount",
            "trackedSince",
            "options",
            "failureMessage",
            "createStatement",
        ]),
        (Some(ShowTargetKind::Constraints), false, _) => Some(&[
            "id",
            "name",
            "type",
            "entityType",
            "labelsOrTypes",
            "properties",
            "enforcedLabel",
            "ownedIndex",
            "propertyType",
        ]),
        (Some(ShowTargetKind::Constraints), true, _) => Some(&[
            "id",
            "name",
            "type",
            "entityType",
            "labelsOrTypes",
            "properties",
            "enforcedLabel",
            "classification",
            "ownedIndex",
            "propertyType",
            "options",
            "createStatement",
        ]),
        _ => None,
    }
}

fn logical_operators(root: &AstNode) -> Vec<LogicalOperator> {
    let mut operators = Vec::new();
    let mut has_read_rows = false;
    for node in root.descendants() {
        match node.kind {
            AstKind::Connector(crate::cypher::QueryConnector::Union)
            | AstKind::Connector(crate::cypher::QueryConnector::UnionDistinct) => {
                operators.push(LogicalOperator::Union { distinct: true });
            }
            AstKind::Connector(crate::cypher::QueryConnector::UnionAll) => {
                operators.push(LogicalOperator::Union { distinct: false });
            }
            AstKind::Connector(crate::cypher::QueryConnector::Next) => {
                operators.push(LogicalOperator::Next);
            }
            AstKind::ConditionalQuery => operators.push(LogicalOperator::When),
            AstKind::Clause(ClauseKind::Match | ClauseKind::OptionalMatch) => {
                append_program_match_operators(node, &mut operators);
                has_read_rows = true;
            }
            AstKind::Clause(ClauseKind::Filter) => {
                operators.push(LogicalOperator::Filter);
                has_read_rows = true;
            }
            AstKind::Clause(ClauseKind::Return | ClauseKind::With) => {
                append_program_projection_operators(node, &mut operators);
                has_read_rows = true;
            }
            AstKind::Clause(ClauseKind::Let) => {
                operators.push(LogicalOperator::Let);
                has_read_rows = true;
            }
            AstKind::Clause(ClauseKind::Unwind | ClauseKind::For) => {
                operators.push(LogicalOperator::Unwind);
                has_read_rows = true;
            }
            AstKind::Clause(ClauseKind::Call) => {
                operators.push(LogicalOperator::Subquery);
                has_read_rows = true;
            }
            AstKind::Clause(kind)
                if matches!(
                    kind,
                    ClauseKind::Create
                        | ClauseKind::Insert
                        | ClauseKind::Merge
                        | ClauseKind::Set
                        | ClauseKind::Remove
                        | ClauseKind::Delete
                        | ClauseKind::DetachDelete
                        | ClauseKind::Foreach
                ) =>
            {
                if has_read_rows {
                    operators.push(LogicalOperator::Eager);
                }
                operators.push(LogicalOperator::Mutation { kind });
                has_read_rows = true;
            }
            _ => {}
        }
    }
    if !operators
        .iter()
        .any(|operator| matches!(operator, LogicalOperator::Project))
    {
        operators.push(LogicalOperator::Project);
    }
    if contains_mutation(root) {
        operators.push(LogicalOperator::Commit);
    }
    operators
}

fn append_program_match_operators(clause: &AstNode, operators: &mut Vec<LogicalOperator>) {
    if clause.kind == AstKind::Clause(ClauseKind::OptionalMatch) {
        operators.push(LogicalOperator::Optional);
    }
    if let Some(search) = clause
        .descendants()
        .find(|node| node.kind == AstKind::Search)
    {
        let variable = search
            .descendants()
            .find(|node| node.kind == AstKind::Variable)
            .and_then(|node| node.text.as_deref())
            .map(crate::cypher::unescape_identifier)
            .unwrap_or_else(|| "_search".to_owned());
        let index = search
            .descendants()
            .find(|node| node.kind == AstKind::IndexName)
            .and_then(|node| node.text.as_deref())
            .map(crate::cypher::unescape_identifier)
            .unwrap_or_else(|| "_vector_index".to_owned());
        operators.push(LogicalOperator::IndexSeek {
            variable,
            index,
            kind: crate::storage::StandardIndexKind::Vector,
        });
    }
    for part in clause
        .descendants()
        .filter(|node| node.kind == AstKind::PatternPart)
    {
        let nodes = part
            .descendants()
            .filter(|node| node.kind == AstKind::NodePattern)
            .collect::<Vec<_>>();
        let Some(start) = nodes.first() else {
            continue;
        };
        let variable = pattern_variable(start, "_anon");
        if let Some(label) = start
            .descendants()
            .find(|node| node.kind == AstKind::LabelName)
            .and_then(|node| node.text.clone())
        {
            operators.push(LogicalOperator::LabelScan { variable, label });
        } else {
            operators.push(LogicalOperator::NodeScan { variable });
        }
        for (index, _) in part
            .descendants()
            .filter(|node| node.kind == AstKind::RelationshipPattern)
            .enumerate()
        {
            operators.push(LogicalOperator::ExpandAll {
                from: pattern_variable(nodes[index], "_anon"),
                relationship: None,
                to: pattern_variable(nodes[index + 1], "_anon_target"),
            });
        }
    }
    if clause.descendants().any(|node| node.kind == AstKind::Where) {
        operators.push(LogicalOperator::Filter);
    }
}

fn pattern_variable(node: &AstNode, fallback: &str) -> String {
    node.descendants()
        .find(|node| node.kind == AstKind::PatternVariable)
        .and_then(|node| node.text.clone())
        .unwrap_or_else(|| fallback.to_owned())
}

fn append_program_projection_operators(clause: &AstNode, operators: &mut Vec<LogicalOperator>) {
    let aggregate = projection_has_aggregate(clause);
    if aggregate {
        operators.push(LogicalOperator::Aggregate);
    }
    if projection_has_distinct(clause) {
        operators.push(LogicalOperator::Distinct);
    }
    operators.push(LogicalOperator::Project);
    if clause
        .descendants()
        .any(|node| node.kind == AstKind::OrderBy)
    {
        operators.push(LogicalOperator::Sort);
    }
    if clause.descendants().any(|node| node.kind == AstKind::Skip) {
        operators.push(LogicalOperator::Skip);
    }
    if clause.descendants().any(|node| node.kind == AstKind::Limit) {
        operators.push(LogicalOperator::Limit);
    }
    if clause.descendants().any(|node| node.kind == AstKind::Where) {
        operators.push(LogicalOperator::Filter);
    }
}

pub(crate) fn projection_requires_global_input(clause: &AstNode) -> bool {
    projection_has_aggregate(clause)
        || projection_has_distinct(clause)
        || clause
            .descendants()
            .any(|node| matches!(node.kind, AstKind::OrderBy | AstKind::Skip | AstKind::Limit))
}

fn projection_has_aggregate(clause: &AstNode) -> bool {
    clause.descendants().any(|node| {
        node.kind == AstKind::GroupBy
            || (node.kind == AstKind::FunctionName
                && node
                    .text
                    .as_deref()
                    .is_some_and(super::registry::is_aggregating))
    })
}

fn projection_has_distinct(clause: &AstNode) -> bool {
    let first_item = clause
        .descendants()
        .find(|node| node.kind == AstKind::ProjectionItem)
        .map_or(clause.span.end, |node| node.span.start);
    clause.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::SetQuantifier(crate::cypher::SetQuantifierKind::Distinct)
        ) && node.span.start < first_item
    })
}

fn physical_operator(operator: &LogicalOperator) -> PhysicalOperator {
    match operator {
        LogicalOperator::Union { distinct } => PhysicalOperator::Union {
            distinct: *distinct,
        },
        LogicalOperator::Next => PhysicalOperator::Next,
        LogicalOperator::When => PhysicalOperator::When,
        LogicalOperator::Let => PhysicalOperator::Let,
        LogicalOperator::Unwind => PhysicalOperator::Unwind,
        LogicalOperator::Subquery => PhysicalOperator::Subquery,
        LogicalOperator::Aggregate => PhysicalOperator::Aggregate,
        LogicalOperator::NodeScan { variable } => PhysicalOperator::NodeScan {
            variable: variable.clone(),
        },
        LogicalOperator::LabelScan { variable, label } => PhysicalOperator::LabelIndexScan {
            variable: variable.clone(),
            label: label.clone(),
        },
        LogicalOperator::IndexSeek {
            variable,
            index,
            kind,
        } if *kind == crate::storage::StandardIndexKind::Vector => PhysicalOperator::VectorSearch {
            variable: variable.clone(),
            index: index.clone(),
        },
        LogicalOperator::IndexSeek {
            variable,
            index,
            kind,
        } => PhysicalOperator::IndexSeek {
            variable: variable.clone(),
            index: index.clone(),
            kind: *kind,
        },
        LogicalOperator::ExpandAll { from, .. } | LogicalOperator::ExpandInto { from, .. } => {
            PhysicalOperator::AdjacencySeek {
                from: from.clone(),
                relationship_type: None,
                direction: super::plan::Direction::Outgoing,
            }
        }
        LogicalOperator::Filter => PhysicalOperator::Filter,
        LogicalOperator::Distinct => PhysicalOperator::Distinct,
        LogicalOperator::Project => PhysicalOperator::Project,
        LogicalOperator::Sort => PhysicalOperator::ExternalSort,
        LogicalOperator::Skip => PhysicalOperator::Skip,
        LogicalOperator::Limit => PhysicalOperator::Limit,
        LogicalOperator::Eager => PhysicalOperator::Eager,
        LogicalOperator::Optional => PhysicalOperator::Optional,
        LogicalOperator::Cartesian => PhysicalOperator::Cartesian,
        LogicalOperator::Mutation { kind } => PhysicalOperator::Mutation { kind: *kind },
        LogicalOperator::Schema { kind } => PhysicalOperator::Schema { kind: *kind },
        LogicalOperator::Commit => PhysicalOperator::Commit,
        LogicalOperator::RelationshipScan { variable } => PhysicalOperator::RelationshipScan {
            variable: variable.clone(),
        },
        LogicalOperator::TypeSeek { relationship_type } => PhysicalOperator::AdjacencySeek {
            from: String::new(),
            relationship_type: Some(relationship_type.clone()),
            direction: super::plan::Direction::Outgoing,
        },
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the program boundary keeps all immutable query execution inputs explicit"
)]
pub(crate) fn execute_prepared_read(
    connection: &Connection,
    program: &PreparedProgram,
    commit: HashId,
    graph_view: &ResolvedGraphView,
    params: &BTreeMap<String, Value>,
    mode: ExecutionMode,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<Vec<Value>>> {
    if program.writes {
        return Err(QueryError::internal(
            "mutating Phase 06 program reached the read executor",
        ));
    }
    if mode == ExecutionMode::Explain {
        return Ok(vec![vec![Value::String(program.physical.explain())]]);
    }
    execute_read(
        connection,
        program,
        commit,
        graph_view,
        params,
        metrics,
        is_interrupted,
    )
}
