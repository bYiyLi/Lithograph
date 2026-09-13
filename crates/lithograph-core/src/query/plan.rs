use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{
    self, AstKind, AstNode, ClauseKind, ExecutionMode, ExpressionKind, NameExpressionKind,
    OrderDirectionKind, SetQuantifierKind, Value,
};
use crate::storage::{self, HashId, LabelId, RelationshipTypeId, StandardIndexKind};

use super::expression::{Expr, compile_expression, is_count, surface_expressions};
use super::graph::{ResolvedGraphView, resolve_commit};
use super::stats::PlannerStatistics;
use super::{ExecutionOptions, QueryError, QueryResult};

mod build;
pub(crate) use build::append_pattern_operators;
use build::{build_logical, build_physical};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LogicalOperator {
    NodeScan {
        variable: String,
    },
    LabelScan {
        variable: String,
        label: String,
    },
    IndexSeek {
        variable: String,
        index: String,
        kind: StandardIndexKind,
    },
    RelationshipScan {
        variable: Option<String>,
    },
    TypeSeek {
        relationship_type: String,
    },
    ExpandAll {
        from: String,
        relationship: Option<String>,
        to: String,
    },
    ExpandInto {
        from: String,
        relationship: Option<String>,
        to: String,
    },
    Filter,
    Project,
    Sort,
    Skip,
    Limit,
    Aggregate,
    Distinct,
    Optional,
    Cartesian,
    Let,
    Unwind,
    Union {
        distinct: bool,
    },
    Subquery,
    When,
    Next,
    Eager,
    Mutation {
        kind: ClauseKind,
    },
    Schema {
        kind: ClauseKind,
    },
    Commit,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LogicalPlan {
    pub operators: Vec<LogicalOperator>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PhysicalOperator {
    NodeScan {
        variable: String,
    },
    LabelIndexScan {
        variable: String,
        label: String,
    },
    IndexSeek {
        variable: String,
        index: String,
        kind: StandardIndexKind,
    },
    VectorSearch {
        variable: String,
        index: String,
    },
    RelationshipScan {
        variable: Option<String>,
    },
    AdjacencySeek {
        from: String,
        relationship_type: Option<String>,
        direction: Direction,
    },
    Filter,
    Project,
    ExternalSort,
    Skip,
    Limit,
    Aggregate,
    Distinct,
    Optional,
    Cartesian,
    Let,
    Unwind,
    Union {
        distinct: bool,
    },
    Subquery,
    When,
    Next,
    Eager,
    Mutation {
        kind: ClauseKind,
    },
    Schema {
        kind: ClauseKind,
    },
    Commit,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Outgoing,
    Incoming,
    Undirected,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PhysicalPlan {
    pub operators: Vec<PhysicalOperator>,
}

impl PhysicalPlan {
    pub fn explain(&self) -> String {
        self.operators
            .iter()
            .enumerate()
            .map(|(index, operator)| format!("{index:02} {operator:?}"))
            .collect::<Vec<_>>()
            .join("\n")
    }
}

#[derive(Debug, Clone)]
pub(crate) struct NodeSpec {
    pub variable: Option<String>,
    pub labels: Vec<LabelId>,
    pub label_names: Vec<String>,
    pub scan_label: Option<LabelId>,
    pub scan_label_name: Option<String>,
    pub index_seek: Option<super::schema::StandardIndexSeek>,
    pub impossible: bool,
}
#[derive(Debug, Clone)]
pub(crate) struct RelationshipSpec {
    pub variable: Option<String>,
    pub type_id: Option<RelationshipTypeId>,
    pub type_name: Option<String>,
    pub direction: Direction,
    pub index_seek: Option<super::schema::StandardIndexSeek>,
    pub impossible: bool,
}
#[derive(Debug, Clone)]
pub(crate) struct PatternPart {
    pub path_variable: Option<String>,
    pub start: NodeSpec,
    pub relationship: Option<RelationshipSpec>,
    pub end: Option<NodeSpec>,
}
#[derive(Debug, Clone)]
pub(crate) struct MatchStep {
    pub optional: bool,
    pub parts: Vec<PatternPart>,
    pub predicate: Option<Expr>,
}
#[derive(Debug, Clone)]
pub(crate) struct Projection {
    pub column: String,
    pub expression: Expr,
}
#[derive(Debug, Clone)]
pub(crate) struct OrderItem {
    pub expression: Expr,
    pub descending: bool,
}

pub(crate) struct ProjectionPlan {
    pub(crate) projections: Vec<Projection>,
    pub(crate) order: Vec<OrderItem>,
    pub(crate) skip: usize,
    pub(crate) limit: Option<usize>,
    pub(crate) distinct: bool,
}

#[derive(Debug, Clone)]
pub struct PreparedQuery {
    pub(crate) commit: HashId,
    pub(crate) graph_view: ResolvedGraphView,
    pub(crate) matches: Vec<MatchStep>,
    pub(crate) projections: Vec<Projection>,
    pub(crate) order: Vec<OrderItem>,
    pub(crate) skip: usize,
    pub(crate) limit: Option<usize>,
    pub(crate) distinct: bool,
    pub(crate) aggregate: bool,
    pub(crate) params: BTreeMap<String, Value>,
    pub(crate) mode: ExecutionMode,
    pub(crate) write: Option<super::mutation::PreparedWrite>,
    pub(crate) schema: Option<super::schema::PreparedSchema>,
    pub(crate) program: Option<super::completeness::PreparedProgram>,
    pub logical: LogicalPlan,
    pub physical: PhysicalPlan,
    pub statistics: PlannerStatistics,
    pub columns: Vec<String>,
}

impl PreparedQuery {
    pub fn requires_transaction_boundary(&self) -> bool {
        self.program
            .as_ref()
            .is_some_and(|program| program.transaction_options.is_some())
    }

    pub fn has_external_io(&self) -> bool {
        self.program.as_ref().is_some_and(|program| {
            program
                .root
                .descendants()
                .any(|node| node.kind == AstKind::Clause(ClauseKind::LoadCsv))
        })
    }
}

struct PrepareContext {
    commit: HashId,
    graph_view: ResolvedGraphView,
    params: BTreeMap<String, Value>,
    mode: ExecutionMode,
}

pub fn prepare(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> QueryResult<PreparedQuery> {
    let ast = cypher::parse(query)?;
    cypher::analyze(&ast, query)?;
    validate_parameters(&ast.root, &params)?;
    let commit = resolve_commit(connection, &options.snapshot)?;
    let graph_view = ResolvedGraphView::resolve(connection, &options.graph_view)?;
    let context = PrepareContext {
        commit,
        graph_view,
        params,
        mode: ast.execution_mode,
    };
    if let Some(schema) =
        super::schema::prepare_schema(connection, context.commit, &ast, query, &options)?
    {
        return Ok(schema_query(
            context.commit,
            context.graph_view,
            context.params,
            context.mode,
            schema,
        ));
    }
    if super::completeness::requires_program(&ast.root) {
        let program = super::completeness::prepare_program(&ast, query, &options)?;
        return Ok(program_query(
            context.commit,
            context.graph_view,
            context.params,
            context.mode,
            program,
        ));
    }
    reject_query_composition(&ast.root)?;
    let single = single_query(&ast.root)?;
    if contains_mutation(single) {
        return prepare_write_query(connection, single, query, &options, context);
    }
    prepare_read_query(connection, single, query, context)
}

fn prepare_write_query(
    connection: &Connection,
    single: &AstNode,
    query: &str,
    options: &ExecutionOptions,
    context: PrepareContext,
) -> QueryResult<PreparedQuery> {
    let write =
        super::mutation::prepare_write(connection, single, query, &context.params, options)?;
    let columns = if context.mode == ExecutionMode::Explain {
        vec!["plan".to_owned()]
    } else {
        write.columns.clone()
    };
    let logical = write.logical.clone();
    let physical = build_physical(&logical, &write.matches_for_explain, false);
    Ok(PreparedQuery {
        commit: context.commit,
        graph_view: context.graph_view,
        matches: Vec::new(),
        projections: Vec::new(),
        order: Vec::new(),
        skip: 0,
        limit: None,
        distinct: false,
        aggregate: false,
        params: context.params,
        mode: context.mode,
        write: Some(write),
        schema: None,
        program: None,
        logical,
        physical,
        statistics: PlannerStatistics::default(),
        columns,
    })
}

fn prepare_read_query(
    connection: &Connection,
    single: &AstNode,
    query: &str,
    context: PrepareContext,
) -> QueryResult<PreparedQuery> {
    let (mut matches, return_clause) = lower_clauses(connection, single)?;
    let ProjectionPlan {
        projections,
        order,
        skip,
        limit,
        distinct,
    } = lower_projection(return_clause, query, &context.params)?;
    let aggregate = validate_aggregation(&projections)?;
    let statistics = planner_statistics(connection, context.commit, &context.graph_view, &matches)?;
    optimize_node_scans(&mut matches, &statistics);
    super::schema::select_standard_index_seeks(
        connection,
        context.commit,
        &mut matches,
        &context.params,
    )?;
    let columns = output_columns(context.mode, &projections);
    let logical = build_logical(
        &matches,
        !order.is_empty(),
        skip > 0,
        limit.is_some(),
        distinct,
        aggregate,
    );
    let physical = build_physical(&logical, &matches, !order.is_empty());
    Ok(PreparedQuery {
        commit: context.commit,
        graph_view: context.graph_view,
        matches,
        projections,
        order,
        skip,
        limit,
        distinct,
        aggregate,
        params: context.params,
        mode: context.mode,
        write: None,
        schema: None,
        program: None,
        logical,
        physical,
        statistics,
        columns,
    })
}

fn program_query(
    commit: HashId,
    graph_view: ResolvedGraphView,
    params: BTreeMap<String, Value>,
    mode: ExecutionMode,
    program: super::completeness::PreparedProgram,
) -> PreparedQuery {
    let columns = if mode == ExecutionMode::Explain {
        vec!["plan".to_owned()]
    } else {
        program.columns.clone()
    };
    PreparedQuery {
        commit,
        graph_view,
        matches: Vec::new(),
        projections: Vec::new(),
        order: Vec::new(),
        skip: 0,
        limit: None,
        distinct: false,
        aggregate: false,
        params,
        mode,
        write: None,
        schema: None,
        logical: program.logical.clone(),
        physical: program.physical.clone(),
        statistics: PlannerStatistics::default(),
        columns,
        program: Some(program),
    }
}

fn schema_query(
    commit: HashId,
    graph_view: ResolvedGraphView,
    params: BTreeMap<String, Value>,
    mode: ExecutionMode,
    schema: super::schema::PreparedSchema,
) -> PreparedQuery {
    let kind = schema.kind;
    let logical = LogicalPlan {
        operators: vec![LogicalOperator::Schema { kind }, LogicalOperator::Commit],
    };
    let physical = PhysicalPlan {
        operators: vec![PhysicalOperator::Schema { kind }, PhysicalOperator::Commit],
    };
    PreparedQuery {
        commit,
        graph_view,
        matches: Vec::new(),
        projections: Vec::new(),
        order: Vec::new(),
        skip: 0,
        limit: None,
        distinct: false,
        aggregate: false,
        params,
        mode,
        write: None,
        schema: Some(schema),
        program: None,
        logical,
        physical,
        statistics: PlannerStatistics::default(),
        columns: if mode == ExecutionMode::Explain {
            vec!["plan".to_owned()]
        } else {
            Vec::new()
        },
    }
}

fn contains_mutation(single: &AstNode) -> bool {
    single.children.iter().any(|clause| {
        matches!(
            clause.kind,
            AstKind::Clause(
                ClauseKind::Create
                    | ClauseKind::Insert
                    | ClauseKind::Merge
                    | ClauseKind::Set
                    | ClauseKind::Remove
                    | ClauseKind::Delete
                    | ClauseKind::DetachDelete
            )
        )
    })
}

fn validate_parameters(root: &AstNode, params: &BTreeMap<String, Value>) -> QueryResult<()> {
    for parameter in root
        .descendants()
        .filter(|node| matches!(node.kind, AstKind::Parameter))
    {
        let name = parameter
            .text
            .as_deref()
            .unwrap_or_default()
            .trim_start_matches('$');
        if !params.contains_key(name) {
            return Err(QueryError::invalid_argument(format!(
                "parameter ${name} was not provided"
            )));
        }
    }
    Ok(())
}

fn reject_query_composition(root: &AstNode) -> QueryResult<()> {
    if root
        .descendants()
        .any(|node| matches!(node.kind, AstKind::Connector(_)))
    {
        return Err(QueryError::semantic(
            "Phase 04 read execution supports one SingleQuery; query composition is owned by later phases",
        ));
    }
    Ok(())
}

fn single_query(root: &AstNode) -> QueryResult<&AstNode> {
    root.descendants()
        .find(|node| matches!(node.kind, AstKind::SingleQuery))
        .ok_or_else(|| QueryError::semantic("read query is missing a SingleQuery"))
}

fn lower_clauses<'a>(
    connection: &Connection,
    single: &'a AstNode,
) -> QueryResult<(Vec<MatchStep>, &'a AstNode)> {
    let mut matches = Vec::new();
    let mut return_clause = None;
    for clause in &single.children {
        let AstKind::Clause(kind) = clause.kind else {
            continue;
        };
        lower_clause(connection, kind, clause, &mut matches, &mut return_clause)?;
    }
    let return_clause = return_clause
        .ok_or_else(|| QueryError::semantic("Phase 04 read execution requires RETURN"))?;
    Ok((matches, return_clause))
}

fn lower_clause<'a>(
    connection: &Connection,
    kind: ClauseKind,
    clause: &'a AstNode,
    matches: &mut Vec<MatchStep>,
    return_clause: &mut Option<&'a AstNode>,
) -> QueryResult<()> {
    match kind {
        ClauseKind::Match => matches.push(lower_match(connection, clause, false)?),
        ClauseKind::OptionalMatch => matches.push(lower_match(connection, clause, true)?),
        ClauseKind::Return if return_clause.is_none() => *return_clause = Some(clause),
        ClauseKind::Return => {
            return Err(QueryError::semantic(
                "read query contains multiple RETURN clauses",
            ));
        }
        _ => {
            return Err(QueryError::semantic(format!(
                "Phase 04 read execution does not execute {kind:?} clauses"
            )));
        }
    }
    Ok(())
}

fn validate_aggregation(projections: &[Projection]) -> QueryResult<bool> {
    let aggregate = projections
        .iter()
        .any(|projection| is_count(&projection.expression));
    if aggregate
        && projections
            .iter()
            .any(|projection| !is_count(&projection.expression))
    {
        return Err(QueryError::semantic(
            "Phase 04 basic aggregation supports global count projections; grouped aggregation is owned by Phase 06",
        ));
    }
    Ok(aggregate)
}

fn planner_statistics(
    connection: &Connection,
    commit: HashId,
    graph_view: &ResolvedGraphView,
    matches: &[MatchStep],
) -> QueryResult<PlannerStatistics> {
    if matches.is_empty() || graph_view.is_empty() {
        Ok(PlannerStatistics::default())
    } else {
        collect_statistics(connection, commit, matches)
    }
}

fn output_columns(mode: ExecutionMode, projections: &[Projection]) -> Vec<String> {
    if mode == ExecutionMode::Explain {
        vec!["plan".to_owned()]
    } else {
        projections
            .iter()
            .map(|projection| projection.column.clone())
            .collect()
    }
}

pub(crate) fn lower_match(
    connection: &Connection,
    clause: &AstNode,
    optional: bool,
) -> QueryResult<MatchStep> {
    if clause
        .descendants()
        .any(|node| matches!(node.kind, AstKind::MatchMode(_) | AstKind::Search))
    {
        return Err(QueryError::semantic(
            "Phase 04 read execution does not support explicit MATCH modes or SEARCH subclauses",
        ));
    }
    let pattern = clause
        .children
        .iter()
        .find(|node| matches!(node.kind, AstKind::Pattern))
        .or_else(|| {
            clause
                .descendants()
                .find(|node| matches!(node.kind, AstKind::Pattern))
        })
        .ok_or_else(|| QueryError::semantic("MATCH clause is missing its pattern"))?;
    let mut parts = pattern
        .children
        .iter()
        .filter(|node| matches!(node.kind, AstKind::PatternPart))
        .map(|part| lower_pattern_part(connection, part))
        .collect::<QueryResult<Vec<_>>>()?;
    if parts.is_empty() {
        parts = pattern
            .descendants()
            .filter(|node| matches!(node.kind, AstKind::PatternPart))
            .map(|part| lower_pattern_part(connection, part))
            .collect::<QueryResult<Vec<_>>>()?;
    }
    let predicate = clause
        .descendants()
        .find(|node| matches!(node.kind, AstKind::Where))
        .map(|where_node| first_surface_expression(where_node).and_then(compile_expression))
        .transpose()?;
    Ok(MatchStep {
        optional,
        parts,
        predicate,
    })
}

fn lower_pattern_part(connection: &Connection, part: &AstNode) -> QueryResult<PatternPart> {
    if part.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::PathMode(_) | AstKind::PathSelector(_) | AstKind::Quantifier(_)
        )
    }) {
        return Err(QueryError::semantic(
            "Phase 04 fixed-path execution does not support path modes, selectors, or quantified patterns",
        ));
    }
    let path_variable = part
        .children
        .iter()
        .find(|node| matches!(node.kind, AstKind::PathAssignment))
        .and_then(|assignment| {
            assignment
                .descendants()
                .find(|node| matches!(node.kind, AstKind::PatternVariable))
        })
        .and_then(|node| node.text.clone());
    let mut nodes = part
        .descendants()
        .filter(|node| matches!(node.kind, AstKind::NodePattern))
        .collect::<Vec<_>>();
    nodes.sort_by_key(|node| node.span.start);
    let mut relationships = part
        .descendants()
        .filter(|node| matches!(node.kind, AstKind::RelationshipPattern))
        .collect::<Vec<_>>();
    relationships.sort_by_key(|node| node.span.start);
    if nodes.is_empty() {
        return Err(QueryError::semantic(
            "pattern part is missing a Node pattern",
        ));
    }
    if relationships.len() > 1 || nodes.len() > 2 {
        return Err(QueryError::semantic(
            "Phase 04 fixed-path execution supports one Relationship hop per PatternPart",
        ));
    }
    let start = lower_node(connection, nodes[0])?;
    if relationships.is_empty() {
        return Ok(PatternPart {
            path_variable,
            start,
            relationship: None,
            end: None,
        });
    }
    if nodes.len() != 2 {
        return Err(QueryError::semantic(
            "Relationship pattern must connect two Node patterns",
        ));
    }
    Ok(PatternPart {
        path_variable,
        start,
        relationship: Some(lower_relationship(connection, relationships[0])?),
        end: Some(lower_node(connection, nodes[1])?),
    })
}

fn lower_node(connection: &Connection, node: &AstNode) -> QueryResult<NodeSpec> {
    reject_unsupported_pattern_predicates(node, "Node")?;
    validate_name_expression(node, true)?;
    let variable = node
        .descendants()
        .skip(1)
        .find(|child| matches!(child.kind, AstKind::PatternVariable | AstKind::Variable))
        .and_then(|child| child.text.clone());
    let parsed_label_names = node
        .descendants()
        .skip(1)
        .filter(|child| matches!(child.kind, AstKind::LabelName))
        .filter_map(|child| child.text.clone())
        .collect::<Vec<_>>();
    let mut labels = Vec::new();
    let mut label_names = Vec::new();
    let mut impossible = false;
    for name in parsed_label_names {
        match storage::find_label(connection, &name)? {
            Some(id) => {
                labels.push(id);
                label_names.push(name);
            }
            None => impossible = true,
        }
    }
    Ok(NodeSpec {
        variable,
        scan_label: labels.first().copied(),
        scan_label_name: label_names.first().cloned(),
        index_seek: None,
        labels,
        label_names,
        impossible,
    })
}

fn lower_relationship(connection: &Connection, node: &AstNode) -> QueryResult<RelationshipSpec> {
    reject_unsupported_pattern_predicates(node, "Relationship")?;
    validate_name_expression(node, false)?;
    if node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::VariableLength))
    {
        return Err(QueryError::semantic(
            "Phase 04 fixed Relationship patterns do not support variable-length traversal",
        ));
    }
    let variable = node
        .descendants()
        .skip(1)
        .find(|child| {
            matches!(
                child.kind,
                AstKind::RelationshipVariable | AstKind::Variable
            )
        })
        .and_then(|child| child.text.clone());
    let types = node
        .descendants()
        .skip(1)
        .filter(|child| matches!(child.kind, AstKind::RelationshipTypeName))
        .filter_map(|child| child.text.clone())
        .collect::<Vec<_>>();
    if types.len() > 1 {
        return Err(QueryError::semantic(
            "Phase 04 fixed Relationship pattern supports one Relationship Type",
        ));
    }
    let type_name = types.first().cloned();
    let type_id = type_name
        .as_deref()
        .map(|name| storage::find_relationship_type(connection, name))
        .transpose()?
        .flatten();
    let impossible = type_name.is_some() && type_id.is_none();
    let left = node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::RelationshipLeftArrow));
    let right = node
        .descendants()
        .any(|child| matches!(child.kind, AstKind::RelationshipRightArrow));
    let direction = match (left, right) {
        (false, true) => Direction::Outgoing,
        (true, false) => Direction::Incoming,
        _ => Direction::Undirected,
    };
    Ok(RelationshipSpec {
        variable,
        type_id,
        type_name,
        direction,
        index_seek: None,
        impossible,
    })
}

fn reject_unsupported_pattern_predicates(node: &AstNode, element: &str) -> QueryResult<()> {
    if node
        .descendants()
        .skip(1)
        .any(|child| matches!(child.kind, AstKind::Where))
    {
        return Err(QueryError::semantic(format!(
            "Phase 04 {element} patterns do not support inline WHERE predicates; use MATCH ... WHERE instead"
        )));
    }
    if node
        .descendants()
        .skip(1)
        .any(|child| matches!(child.kind, AstKind::Expression(ExpressionKind::Map)))
    {
        return Err(QueryError::semantic(format!(
            "Phase 04 {element} patterns do not support inline property maps; use MATCH ... WHERE instead"
        )));
    }
    Ok(())
}

fn validate_name_expression(node: &AstNode, node_labels: bool) -> QueryResult<()> {
    for child in node.descendants().skip(1) {
        match child.kind {
            AstKind::NameExpression(NameExpressionKind::Negation(count)) if count > 0 => {
                return Err(QueryError::semantic(
                    "Phase 04 pattern execution does not support negated label/type expressions",
                ));
            }
            AstKind::NameExpression(NameExpressionKind::Dynamic | NameExpressionKind::Wildcard) => {
                return Err(QueryError::semantic(
                    "Phase 04 pattern execution does not support dynamic or wildcard label/type expressions",
                ));
            }
            AstKind::NameExpression(NameExpressionKind::Disjunction)
                if child.children.len() > 1 =>
            {
                return Err(QueryError::semantic(
                    "Phase 04 pattern execution does not support disjunctive label/type expressions",
                ));
            }
            AstKind::NameExpression(NameExpressionKind::Conjunction)
                if !node_labels && child.children.len() > 1 =>
            {
                return Err(QueryError::semantic(
                    "Phase 04 Relationship patterns support one Relationship Type",
                ));
            }
            _ => {}
        }
    }
    Ok(())
}

pub(crate) fn lower_projection(
    clause: &AstNode,
    source: &str,
    params: &BTreeMap<String, Value>,
) -> QueryResult<ProjectionPlan> {
    let body = clause
        .descendants()
        .find(|n| matches!(n.kind, AstKind::ProjectionBody))
        .ok_or_else(|| QueryError::semantic("RETURN is missing its projection body"))?;
    if body
        .descendants()
        .any(|n| matches!(n.kind, AstKind::StarProjection))
    {
        return Err(QueryError::semantic(
            "RETURN * is outside Phase 04 execution scope",
        ));
    }
    let mut items = body
        .descendants()
        .filter(|n| matches!(n.kind, AstKind::ProjectionItem))
        .collect::<Vec<_>>();
    items.sort_by_key(|n| n.span.start);
    let projections = items
        .into_iter()
        .map(|item| {
            let expression_node = first_surface_expression(item)?;
            let expression = compile_expression(expression_node)?;
            let column = item
                .descendants()
                .find(|n| matches!(n.kind, AstKind::ProjectionAlias))
                .and_then(|n| n.text.clone())
                .or_else(|| {
                    source
                        .get(expression_node.span.start..expression_node.span.end)
                        .map(str::trim)
                        .filter(|text| !text.is_empty())
                        .map(str::to_owned)
                })
                .ok_or_else(|| QueryError::semantic("RETURN item is missing output text"))?;
            Ok(Projection { column, expression })
        })
        .collect::<QueryResult<Vec<_>>>()?;
    if projections.is_empty() {
        return Err(QueryError::semantic(
            "RETURN must project at least one item",
        ));
    }
    let distinct = body
        .descendants()
        .any(|n| matches!(n.kind, AstKind::SetQuantifier(SetQuantifierKind::Distinct)));
    let order = body
        .descendants()
        .find(|n| matches!(n.kind, AstKind::OrderBy))
        .map(lower_order)
        .transpose()?
        .unwrap_or_default();
    let skip = body
        .descendants()
        .find(|n| matches!(n.kind, AstKind::Skip))
        .map(|n| constant_usize(n, params, "SKIP"))
        .transpose()?
        .unwrap_or(0);
    let limit = body
        .descendants()
        .find(|n| matches!(n.kind, AstKind::Limit))
        .map(|n| constant_usize(n, params, "LIMIT"))
        .transpose()?;
    Ok(ProjectionPlan {
        projections,
        order,
        skip,
        limit,
        distinct,
    })
}

fn lower_order(node: &AstNode) -> QueryResult<Vec<OrderItem>> {
    let expressions = surface_expressions(node);
    let mut directions = node
        .descendants()
        .filter_map(|child| match child.kind {
            AstKind::OrderDirection(direction) => Some((child.span.start, direction)),
            _ => None,
        })
        .collect::<Vec<_>>();
    directions.sort_by_key(|entry| entry.0);
    expressions
        .iter()
        .enumerate()
        .map(|(index, expression)| {
            let end = expressions
                .get(index + 1)
                .map_or(node.span.end, |next| next.span.start);
            let descending = directions
                .iter()
                .find(|(position, _)| *position >= expression.span.end && *position < end)
                .is_some_and(|(_, direction)| *direction == OrderDirectionKind::Descending);
            Ok(OrderItem {
                expression: compile_expression(expression)?,
                descending,
            })
        })
        .collect()
}

fn first_surface_expression(node: &AstNode) -> QueryResult<&AstNode> {
    surface_expressions(node)
        .into_iter()
        .next()
        .ok_or_else(|| QueryError::semantic("clause is missing its expression"))
}

fn constant_usize(
    node: &AstNode,
    params: &BTreeMap<String, Value>,
    name: &str,
) -> QueryResult<usize> {
    let expression = compile_expression(first_surface_expression(node)?)?;
    let value = match expression {
        Expr::Literal(Value::Integer(value)) => value,
        Expr::Parameter(key) => match params.get(&key) {
            Some(Value::Integer(value)) => *value,
            _ => {
                return Err(QueryError::new(
                    super::QueryErrorKind::Type,
                    format!("{name} parameter must be Integer"),
                ));
            }
        },
        _ => {
            return Err(QueryError::semantic(format!(
                "Phase 04 {name} supports Integer literals or parameters"
            )));
        }
    };
    usize::try_from(value).map_err(|_| {
        QueryError::new(
            super::QueryErrorKind::Type,
            format!("{name} must be non-negative"),
        )
    })
}

fn collect_statistics(
    connection: &Connection,
    commit: HashId,
    matches: &[MatchStep],
) -> QueryResult<PlannerStatistics> {
    let mut labels = BTreeSet::new();
    let mut relationship_types = BTreeSet::new();
    let mut need_relationships = false;
    for step in matches {
        for part in &step.parts {
            labels.extend(part.start.labels.iter().copied());
            if let Some(end) = &part.end {
                labels.extend(end.labels.iter().copied());
            }
            if let Some(relationship) = &part.relationship {
                need_relationships = true;
                if let Some(type_id) = relationship.type_id {
                    relationship_types.insert(type_id);
                }
            }
        }
    }
    let snapshot = storage::Snapshot::resolve(connection, commit)?;
    PlannerStatistics::collect(
        &snapshot,
        &labels,
        &relationship_types,
        true,
        need_relationships,
    )
}

fn optimize_node_scans(matches: &mut [MatchStep], statistics: &PlannerStatistics) {
    for step in matches {
        for part in &mut step.parts {
            optimize_node_scan(&mut part.start, statistics);
            if let Some(end) = &mut part.end {
                optimize_node_scan(end, statistics);
            }
        }
    }
}

fn optimize_node_scan(node: &mut NodeSpec, statistics: &PlannerStatistics) {
    let best = node
        .labels
        .iter()
        .enumerate()
        .filter_map(|(index, label_id)| {
            statistics
                .label_count(*label_id)
                .map(|count| (index, label_id, count))
        })
        .min_by_key(|(_, _, count)| *count);
    if let Some((index, label_id, _)) = best {
        node.scan_label = Some(*label_id);
        node.scan_label_name = node.label_names.get(index).cloned();
    }
}
