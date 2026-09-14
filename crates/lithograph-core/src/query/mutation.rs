use std::cmp::Ordering;
use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{
    AstKind, AstNode, ClauseKind, MergeActionKind, NameExpressionKind, SetOperatorKind, Value,
    VectorValues,
};
use crate::storage::{
    self, CommitMetadata, HashId, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord,
    Snapshot, VectorCoordinateType as StorageVectorCoordinateType,
    VectorValue as StorageVectorValue,
};

use super::expression::{self, BindingRow, BindingValue, Expr, compile_expression, is_count};
use super::graph::{ResolvedGraphView, property_value};
use super::options::{ExecutionOptions, GraphViewSelector, writable_branch};
use super::plan::{
    Direction, LogicalOperator, LogicalPlan, MatchStep, OrderItem, Projection, ProjectionPlan,
    append_pattern_operators, lower_match, lower_projection,
};
use super::stream::{QueryMetrics, materialize_match_step};
use super::{QueryError, QueryErrorKind, QueryResult, now_micros};
use crate::cypher::unescape_identifier;

const SCAN_BATCH: usize = 256;

mod execute;
pub(crate) use execute::{
    TransactionBatchOutcome, TransactionMutationContext, execute_program,
    execute_program_suffix_transaction, execute_transaction_batch, execute_write,
    property_from_value,
};

#[derive(Debug, Clone)]
pub(crate) struct PreparedWrite {
    clauses: Vec<WriteClause>,
    projection: Option<WriteProjection>,
    graph_view: GraphViewSelector,
    branch: String,
    author: Option<String>,
    message: Option<String>,
    pub(crate) columns: Vec<String>,
    pub(crate) logical: LogicalPlan,
    pub(crate) matches_for_explain: Vec<MatchStep>,
}

#[derive(Debug, Clone)]
enum WriteClause {
    Match {
        clause: AstNode,
        optional: bool,
    },
    Create(Vec<WritePatternPart>),
    Set(Vec<SetItem>),
    Remove(Vec<RemoveItem>),
    Delete {
        expressions: Vec<Expr>,
        detach: bool,
    },
    Merge(MergePlan),
}

#[derive(Debug, Clone)]
struct WriteProjection {
    projections: Vec<Projection>,
    order: Vec<OrderItem>,
    skip: usize,
    limit: Option<usize>,
    distinct: bool,
}

#[derive(Debug, Clone)]
struct WritePatternPart {
    path_variable: Option<String>,
    nodes: Vec<NodeWriteSpec>,
    relationships: Vec<RelationshipWriteSpec>,
}

#[derive(Debug, Clone)]
struct NodeWriteSpec {
    variable: Option<String>,
    labels: Vec<WriteName>,
    properties: Option<Expr>,
}

#[derive(Debug, Clone)]
struct RelationshipWriteSpec {
    variable: Option<String>,
    relationship_type: WriteName,
    direction: Direction,
    properties: Option<Expr>,
}

#[derive(Debug, Clone)]
enum SetItem {
    Property {
        variable: String,
        key: WritePropertyKey,
        value: Expr,
    },
    Properties {
        variable: String,
        operator: SetOperatorKind,
        value: Expr,
    },
    Labels {
        variable: String,
        labels: Vec<WriteName>,
    },
}

#[derive(Debug, Clone)]
enum RemoveItem {
    Property {
        variable: String,
        key: WritePropertyKey,
    },
    Labels {
        variable: String,
        labels: Vec<WriteName>,
    },
}

#[derive(Debug, Clone)]
enum WriteName {
    Static(String),
    Dynamic(Expr),
}

#[derive(Debug, Clone)]
enum WritePropertyKey {
    Static(String),
    Dynamic(Expr),
}

#[derive(Debug, Clone)]
struct MergePlan {
    pattern: WritePatternPart,
    on_create: Vec<SetItem>,
    on_match: Vec<SetItem>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct MutationCounters {
    pub nodes_created: u64,
    pub nodes_deleted: u64,
    pub relationships_created: u64,
    pub relationships_deleted: u64,
    pub properties_set: u64,
    pub properties_removed: u64,
    pub labels_added: u64,
    pub labels_removed: u64,
}

#[derive(Debug, Clone)]
pub(crate) struct WriteOutcome {
    pub rows: Vec<Vec<Value>>,
    pub commit: HashId,
    pub counters: MutationCounters,
}

#[derive(Debug, Clone)]
struct Slot<T> {
    before: T,
    current: T,
}

#[derive(Debug, Default)]
struct DeltaBuilder {
    nodes: BTreeMap<i64, Slot<bool>>,
    labels: BTreeMap<(i64, i64), Slot<bool>>,
    relationships: BTreeMap<i64, Slot<Option<RelationshipRecord>>>,
    properties: BTreeMap<(OwnerKind, i64, i64), Slot<Option<PropertyValue>>>,
}

struct LoweredClause {
    write: Option<WriteClause>,
    projection: Option<WriteProjection>,
    logical: Vec<LogicalOperator>,
    explain_match: Option<MatchStep>,
}

fn check_interrupted(is_interrupted: &dyn Fn() -> bool) -> QueryResult<()> {
    if is_interrupted() {
        Err(QueryError::interrupted())
    } else {
        Ok(())
    }
}

pub(crate) fn prepare_write(
    connection: &Connection,
    single: &AstNode,
    source: &str,
    params: &BTreeMap<String, Value>,
    options: &ExecutionOptions,
) -> QueryResult<PreparedWrite> {
    let branch = writable_branch(connection, options)?;
    let mut clauses = Vec::new();
    let mut projection = None;
    let mut logical = Vec::new();
    let mut matches_for_explain = Vec::new();
    let mut explain_bound = BTreeSet::new();

    for clause in &single.children {
        let Some(lowered) = lower_plan_clause(connection, clause, source, params)? else {
            continue;
        };
        if let Some(step) = lowered.explain_match.as_ref() {
            if step.optional {
                logical.push(LogicalOperator::Optional);
            }
            append_pattern_operators(step, &mut explain_bound, &mut logical);
            if step.predicate.is_some() {
                logical.push(LogicalOperator::Filter);
            }
        }
        logical.extend(lowered.logical);
        if let Some(write) = lowered.write {
            bind_write_variables(&write, &mut explain_bound);
            clauses.push(write);
        }
        if let Some(return_projection) = lowered.projection {
            projection = Some(return_projection);
        }
        if let Some(step) = lowered.explain_match {
            matches_for_explain.push(step);
        }
    }
    logical.push(LogicalOperator::Commit);
    let columns = projection
        .as_ref()
        .map(|projection| {
            projection
                .projections
                .iter()
                .map(|item| item.column.clone())
                .collect()
        })
        .unwrap_or_default();
    Ok(PreparedWrite {
        clauses,
        projection,
        graph_view: options.graph_view.clone(),
        branch,
        author: options.author.clone(),
        message: options.message.clone(),
        columns,
        logical: LogicalPlan { operators: logical },
        matches_for_explain,
    })
}

fn lower_plan_clause(
    connection: &Connection,
    clause: &AstNode,
    source: &str,
    params: &BTreeMap<String, Value>,
) -> QueryResult<Option<LoweredClause>> {
    let AstKind::Clause(kind) = clause.kind else {
        return Ok(None);
    };
    let lowered = match kind {
        ClauseKind::Match | ClauseKind::OptionalMatch => {
            lower_match_clause(connection, clause, kind)?
        }
        ClauseKind::Create | ClauseKind::Insert => {
            mutation_clause(WriteClause::Create(lower_write_pattern(clause)?), kind)
        }
        ClauseKind::Set => mutation_clause(WriteClause::Set(lower_set_items(clause)?), kind),
        ClauseKind::Remove => {
            mutation_clause(WriteClause::Remove(lower_remove_items(clause)?), kind)
        }
        ClauseKind::Delete | ClauseKind::DetachDelete => mutation_clause(
            WriteClause::Delete {
                expressions: lower_delete_expressions(clause)?,
                detach: kind == ClauseKind::DetachDelete,
            },
            kind,
        ),
        ClauseKind::Merge => mutation_clause(WriteClause::Merge(lower_merge(clause)?), kind),
        ClauseKind::Return => {
            let projection = write_projection(lower_projection(clause, source, params)?)?;
            let logical = write_projection_operators(&projection);
            LoweredClause {
                write: None,
                projection: Some(projection),
                logical,
                explain_match: None,
            }
        }
        ClauseKind::Finish => return Ok(None),
        other => {
            return Err(QueryError::semantic(format!(
                "Phase 05 write execution does not execute {other:?} in a mutating query"
            )));
        }
    };
    Ok(Some(lowered))
}

fn lower_match_clause(
    connection: &Connection,
    clause: &AstNode,
    kind: ClauseKind,
) -> QueryResult<LoweredClause> {
    let optional = kind == ClauseKind::OptionalMatch;
    let explain_match = lower_match(connection, clause, optional)?;
    Ok(LoweredClause {
        write: Some(WriteClause::Match {
            clause: clause.clone(),
            optional,
        }),
        projection: None,
        logical: vec![LogicalOperator::Eager],
        explain_match: Some(explain_match),
    })
}

fn mutation_clause(write: WriteClause, kind: ClauseKind) -> LoweredClause {
    LoweredClause {
        write: Some(write),
        projection: None,
        logical: vec![LogicalOperator::Mutation { kind }, LogicalOperator::Eager],
        explain_match: None,
    }
}

fn write_projection(plan: ProjectionPlan) -> QueryResult<WriteProjection> {
    let projection = WriteProjection {
        projections: plan.projections,
        order: plan.order,
        skip: plan.skip,
        limit: plan.limit,
        distinct: plan.distinct,
    };
    let has_aggregate = projection
        .projections
        .iter()
        .any(|item| is_count(&item.expression));
    if has_aggregate
        && projection
            .projections
            .iter()
            .any(|item| !is_count(&item.expression))
    {
        return Err(QueryError::semantic(
            "Phase 05 write RETURN does not mix aggregate and non-aggregate projections",
        ));
    }
    Ok(projection)
}

fn write_projection_operators(projection: &WriteProjection) -> Vec<LogicalOperator> {
    let mut operators = Vec::new();
    if projection
        .projections
        .iter()
        .all(|item| is_count(&item.expression))
    {
        operators.push(LogicalOperator::Aggregate);
    }
    if projection.distinct {
        operators.push(LogicalOperator::Distinct);
    }
    operators.push(LogicalOperator::Project);
    if !projection.order.is_empty() {
        operators.push(LogicalOperator::Sort);
    }
    if projection.skip > 0 {
        operators.push(LogicalOperator::Skip);
    }
    if projection.limit.is_some() {
        operators.push(LogicalOperator::Limit);
    }
    operators
}

fn bind_write_variables(write: &WriteClause, bound: &mut BTreeSet<String>) {
    let patterns: &[WritePatternPart] = match write {
        WriteClause::Create(patterns) => patterns,
        WriteClause::Merge(merge) => std::slice::from_ref(&merge.pattern),
        WriteClause::Match { .. }
        | WriteClause::Set(_)
        | WriteClause::Remove(_)
        | WriteClause::Delete { .. } => return,
    };
    for pattern in patterns {
        if let Some(variable) = &pattern.path_variable {
            bound.insert(variable.clone());
        }
        bound.extend(
            pattern
                .nodes
                .iter()
                .filter_map(|node| node.variable.clone()),
        );
        bound.extend(
            pattern
                .relationships
                .iter()
                .filter_map(|relationship| relationship.variable.clone()),
        );
    }
}

fn lower_write_pattern(clause: &AstNode) -> QueryResult<Vec<WritePatternPart>> {
    clause
        .descendants()
        .filter(|node| node.kind == AstKind::PatternPart)
        .map(lower_pattern_part)
        .collect()
}

fn lower_pattern_part(part: &AstNode) -> QueryResult<WritePatternPart> {
    if part.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::PathMode(_) | AstKind::PathSelector(_) | AstKind::Quantifier(_)
        )
    }) {
        return Err(QueryError::semantic(
            "Phase 05 mutation patterns do not support path modes, selectors, or quantifiers",
        ));
    }
    let nodes = part
        .descendants()
        .filter(|node| node.kind == AstKind::NodePattern)
        .map(lower_node_write_spec)
        .collect::<QueryResult<Vec<_>>>()?;
    let relationships = part
        .descendants()
        .filter(|node| node.kind == AstKind::RelationshipPattern)
        .map(lower_relationship_write_spec)
        .collect::<QueryResult<Vec<_>>>()?;
    if nodes.is_empty() || relationships.len() + 1 != nodes.len() {
        return Err(QueryError::semantic(
            "Phase 05 mutation patterns require a linear Node/Relationship path",
        ));
    }
    let path_variable = part
        .children
        .iter()
        .find(|node| node.kind == AstKind::PathAssignment)
        .and_then(|node| {
            node.descendants()
                .find(|child| child.kind == AstKind::PatternVariable)
        })
        .and_then(|node| node.text.as_deref())
        .map(unescape_identifier);
    Ok(WritePatternPart {
        path_variable,
        nodes,
        relationships,
    })
}

fn lower_node_write_spec(node: &AstNode) -> QueryResult<NodeWriteSpec> {
    if node.descendants().any(|child| child.kind == AstKind::Where) {
        return Err(QueryError::semantic(
            "Phase 05 Node mutation patterns do not support inline WHERE",
        ));
    }
    let variable = node
        .descendants()
        .find(|child| child.kind == AstKind::PatternVariable)
        .and_then(|child| child.text.as_deref())
        .map(unescape_identifier);
    let labels = lower_write_names(node, AstKind::LabelName)?;
    if node.descendants().any(|child| match child.kind {
        AstKind::NameExpression(
            NameExpressionKind::Wildcard | NameExpressionKind::Negation(1..),
        ) => true,
        AstKind::NameExpression(NameExpressionKind::Disjunction) => child.children.len() > 1,
        _ => false,
    }) {
        return Err(QueryError::semantic(
            "mutation patterns require positive conjunctive or dynamic labels",
        ));
    }
    let properties = node
        .children
        .iter()
        .find(|child| {
            matches!(
                child.kind,
                AstKind::Expression(crate::cypher::ExpressionKind::Map)
            )
        })
        .map(compile_expression)
        .transpose()?;
    Ok(NodeWriteSpec {
        variable,
        labels,
        properties,
    })
}

fn lower_relationship_write_spec(node: &AstNode) -> QueryResult<RelationshipWriteSpec> {
    if node.descendants().any(|child| child.kind == AstKind::Where) {
        return Err(QueryError::semantic(
            "Phase 05 Relationship mutation patterns do not support inline WHERE",
        ));
    }
    let variable = node
        .descendants()
        .find(|child| child.kind == AstKind::RelationshipVariable)
        .and_then(|child| child.text.as_deref())
        .map(unescape_identifier);
    let types = lower_write_names(node, AstKind::RelationshipTypeName)?;
    if node.descendants().any(|child| {
        matches!(
            child.kind,
            AstKind::NameExpression(
                NameExpressionKind::Wildcard | NameExpressionKind::Negation(1..)
            )
        )
    }) {
        return Err(QueryError::semantic(
            "relationship mutations require one positive static or dynamic Relationship Type",
        ));
    }
    if types.len() != 1 {
        return Err(QueryError::semantic(
            "relationship mutations require exactly one Relationship Type expression",
        ));
    }
    if node
        .descendants()
        .any(|child| child.kind == AstKind::VariableLength)
    {
        return Err(QueryError::semantic(
            "variable-length relationship mutation patterns are not supported",
        ));
    }
    let left = node
        .descendants()
        .any(|child| child.kind == AstKind::RelationshipLeftArrow);
    let right = node
        .descendants()
        .any(|child| child.kind == AstKind::RelationshipRightArrow);
    let direction = match (left, right) {
        (false, true) => Direction::Outgoing,
        (true, false) => Direction::Incoming,
        (false, false) => Direction::Undirected,
        (true, true) => {
            return Err(QueryError::semantic(
                "relationship mutation pattern cannot point in both directions",
            ));
        }
    };
    let properties = node
        .descendants()
        .find(|child| {
            matches!(
                child.kind,
                AstKind::Expression(crate::cypher::ExpressionKind::Map)
            )
        })
        .map(compile_expression)
        .transpose()?;
    Ok(RelationshipWriteSpec {
        variable,
        relationship_type: types.into_iter().next().ok_or_else(|| {
            QueryError::semantic("relationship mutation is missing its Relationship Type")
        })?,
        direction,
        properties,
    })
}

fn lower_set_items(clause: &AstNode) -> QueryResult<Vec<SetItem>> {
    clause
        .descendants()
        .filter(|node| node.kind == AstKind::SetItem)
        .map(lower_set_item)
        .collect()
}

fn lower_set_item(item: &AstNode) -> QueryResult<SetItem> {
    if let Some(labels) = item
        .children
        .iter()
        .find(|child| child.kind == AstKind::LabelUpdate)
    {
        return lower_label_set_item(item, labels);
    }
    let operator = item
        .children
        .iter()
        .find_map(|child| match child.kind {
            AstKind::SetOperator(value) => Some(value),
            _ => None,
        })
        .ok_or_else(|| QueryError::semantic("SET item is missing its operator"))?;
    let value = item
        .children
        .iter()
        .find(|child| matches!(child.kind, AstKind::Expression(_)))
        .map(compile_expression)
        .transpose()?
        .ok_or_else(|| QueryError::semantic("SET item is missing its value"))?;
    if let Some(property) = item
        .children
        .iter()
        .find(|child| child.kind == AstKind::AssignableProperty)
    {
        return lower_property_set_item(property, operator, value);
    }
    lower_map_set_item(item, operator, value)
}

fn lower_label_set_item(item: &AstNode, labels: &AstNode) -> QueryResult<SetItem> {
    let variable = direct_variable(item, "SET label item is missing its variable")?;
    let labels = lower_write_names(labels, AstKind::LabelName)?;
    Ok(SetItem::Labels { variable, labels })
}

fn lower_property_set_item(
    property: &AstNode,
    operator: SetOperatorKind,
    value: Expr,
) -> QueryResult<SetItem> {
    if operator != SetOperatorKind::Assign {
        return Err(QueryError::semantic(
            "SET property += is not a Cypher property assignment",
        ));
    }
    let (variable, key) = direct_property_target(
        property,
        "SET property is missing its variable",
        "Phase 05 SET supports direct Node/Relationship properties only",
    )?;
    Ok(SetItem::Property {
        variable,
        key,
        value,
    })
}

fn lower_map_set_item(
    item: &AstNode,
    operator: SetOperatorKind,
    value: Expr,
) -> QueryResult<SetItem> {
    let variable = direct_variable(item, "SET map item is missing its variable")?;
    Ok(SetItem::Properties {
        variable,
        operator,
        value,
    })
}

fn lower_remove_items(clause: &AstNode) -> QueryResult<Vec<RemoveItem>> {
    clause
        .descendants()
        .filter(|node| node.kind == AstKind::RemoveItem)
        .map(|item| {
            if let Some(labels) = item
                .children
                .iter()
                .find(|child| child.kind == AstKind::LabelUpdate)
            {
                let variable = direct_variable(item, "REMOVE label item is missing variable")?;
                let labels = lower_write_names(labels, AstKind::LabelName)?;
                return Ok(RemoveItem::Labels { variable, labels });
            }
            let property = item
                .children
                .iter()
                .find(|child| child.kind == AstKind::PropertyExpression)
                .ok_or_else(|| QueryError::semantic("REMOVE item is unsupported"))?;
            let (variable, key) = direct_property_target(
                property,
                "REMOVE property is missing variable",
                "Phase 05 REMOVE supports direct Node/Relationship properties only",
            )?;
            Ok(RemoveItem::Property { variable, key })
        })
        .collect()
}

fn direct_variable(node: &AstNode, missing: &str) -> QueryResult<String> {
    variable_name(
        node.children
            .iter()
            .find(|child| child.kind == AstKind::Variable),
        missing,
    )
}

fn variable_name(node: Option<&AstNode>, missing: &str) -> QueryResult<String> {
    node.and_then(|child| child.text.as_deref())
        .map(unescape_identifier)
        .ok_or_else(|| QueryError::semantic(missing))
}

fn lower_write_names(node: &AstNode, static_kind: AstKind) -> QueryResult<Vec<WriteName>> {
    let mut names = node
        .descendants()
        .filter_map(|child| {
            if child.kind == static_kind
                && !child.descendants().any(|nested| {
                    nested.kind == AstKind::NameExpression(NameExpressionKind::Dynamic)
                })
            {
                return child
                    .text
                    .as_deref()
                    .map(unescape_identifier)
                    .map(WriteName::Static)
                    .map(Ok);
            }
            if child.kind == AstKind::NameExpression(NameExpressionKind::Dynamic) {
                return child
                    .children
                    .iter()
                    .find(|nested| matches!(nested.kind, AstKind::Expression(_)))
                    .map(compile_expression)
                    .map(|result| result.map(WriteName::Dynamic));
            }
            None
        })
        .collect::<QueryResult<Vec<_>>>()?;
    names.dedup_by(|left, right| {
        matches!((left, right), (WriteName::Static(left), WriteName::Static(right)) if left == right)
    });
    Ok(names)
}

fn resolve_write_names(
    names: &[WriteName],
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
    params: &BTreeMap<String, Value>,
) -> QueryResult<Vec<String>> {
    let mut output = Vec::new();
    for name in names {
        match name {
            WriteName::Static(name) => output.push(name.clone()),
            WriteName::Dynamic(expression) => {
                match expression::evaluate(expression, snapshot, row, params)? {
                    Value::String(value) => output.push(value),
                    Value::List(values) => {
                        for value in values {
                            let Value::String(value) = value else {
                                return Err(QueryError::new(
                                    QueryErrorKind::Type,
                                    "dynamic label/type list must contain only non-null Strings",
                                ));
                            };
                            output.push(value);
                        }
                    }
                    _ => {
                        return Err(QueryError::new(
                            QueryErrorKind::Type,
                            "dynamic label/type expression must be a non-null String or List<String>",
                        ));
                    }
                }
            }
        }
    }
    if output.iter().any(String::is_empty) {
        return Err(QueryError::semantic(
            "dynamic label/type expression cannot produce an empty name",
        ));
    }
    let mut seen = BTreeSet::new();
    output.retain(|name| seen.insert(name.clone()));
    Ok(output)
}

fn direct_property_target(
    node: &AstNode,
    missing_variable: &str,
    unsupported: &str,
) -> QueryResult<(String, WritePropertyKey)> {
    let target = node
        .children
        .iter()
        .find(|child| {
            matches!(
                child.kind,
                AstKind::Variable
                    | AstKind::Expression(crate::cypher::ExpressionKind::Parenthesized)
            )
        })
        .ok_or_else(|| QueryError::semantic(missing_variable))?;
    let Expr::Variable(variable) = compile_expression(target)? else {
        return Err(QueryError::semantic(unsupported));
    };
    let variable = unescape_identifier(&variable);
    let keys = node
        .descendants()
        .filter(|child| child.kind == AstKind::PropertyKey)
        .filter_map(|child| child.text.as_deref())
        .map(unescape_identifier)
        .collect::<Vec<_>>();
    let subscripts = node
        .children
        .iter()
        .filter(|child| child.kind == AstKind::Subscript)
        .collect::<Vec<_>>();
    let key = match (keys.as_slice(), subscripts.as_slice()) {
        ([key], []) => WritePropertyKey::Static(key.clone()),
        ([], [subscript]) => {
            let expression = subscript
                .children
                .iter()
                .find(|child| matches!(child.kind, AstKind::Expression(_)))
                .ok_or_else(|| QueryError::semantic(unsupported))?;
            WritePropertyKey::Dynamic(compile_expression(expression)?)
        }
        _ => return Err(QueryError::semantic(unsupported)),
    };
    Ok((variable, key))
}

fn lower_delete_expressions(clause: &AstNode) -> QueryResult<Vec<Expr>> {
    let expressions = clause
        .descendants()
        .find(|node| node.kind == AstKind::ArgumentList)
        .ok_or_else(|| QueryError::semantic("DELETE is missing its expression list"))?;
    expressions
        .children
        .iter()
        .filter(|node| matches!(node.kind, AstKind::Expression(_)))
        .map(compile_expression)
        .collect()
}

fn lower_merge(clause: &AstNode) -> QueryResult<MergePlan> {
    let pattern = clause
        .children
        .iter()
        .find(|node| node.kind == AstKind::PatternPart)
        .or_else(|| {
            clause
                .descendants()
                .find(|node| node.kind == AstKind::PatternPart)
        })
        .ok_or_else(|| QueryError::semantic("MERGE is missing its pattern"))?;
    let mut on_create = Vec::new();
    let mut on_match = Vec::new();
    for action in clause
        .children
        .iter()
        .filter(|node| matches!(node.kind, AstKind::MergeAction(_)))
    {
        let items = lower_set_items(action)?;
        match action.kind {
            AstKind::MergeAction(MergeActionKind::Create) => on_create.extend(items),
            AstKind::MergeAction(MergeActionKind::Match) => on_match.extend(items),
            _ => unreachable!(),
        }
    }
    Ok(MergePlan {
        pattern: lower_pattern_part(pattern)?,
        on_create,
        on_match,
    })
}
