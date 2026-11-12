use std::collections::{BTreeMap, BTreeSet};

use rusqlite::Connection;

use crate::cypher::{
    self, AstKind, AstNode, ClauseKind, ExecutionMode, ExpressionKind, NameExpressionKind,
    OrderDirectionKind, QueryAst, SetQuantifierKind, Value,
};
use crate::storage::{self, HashId, LabelId, RelationshipTypeId, StandardIndexKind};

use super::completeness::PreparedProgram;
use super::expression::{Expr, compile_expression, is_count, surface_expressions};
use super::graph::{ResolvedGraphView, resolve_commit};
use super::schema::PreparedSchema;
use super::stats::PlannerStatistics;
use super::{AccessPolicy, ExecutionOptions, QueryError, QueryResult};

mod access;
mod budget;
mod build;
pub(crate) mod factor;
pub(crate) use factor::{
    FactorArm, FactorAsp, FactorBindingKind, FactorGroup, FactorHop, FactorizedPlan,
};
pub(crate) mod join;
mod predicate;
mod read;
use read::prepare_read_query;
mod sites;
pub(crate) use sites::{OperatorSite, ProfileBinding, ProfileRole};
#[cfg(test)]
#[path = "plan/statistics_tests.rs"]
mod statistics_tests;
pub(crate) use access::{part_seek_bytes, plan_access, reserve_match_seeks};
pub(crate) use budget::{MAX_DEPTH, PlanningBudget};
use build::build_logical;
pub(crate) use build::build_physical;
pub(crate) use build::{append_match_residual, append_pattern_operators_with_sites};
pub(crate) use predicate::{select_node_id_seeks, select_relationship_id_seeks};

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
    NodeIdSeek {
        variable: String,
    },
    RelationshipScan {
        variable: Option<String>,
    },
    RelationshipIdSeek {
        variable: String,
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
    NestedLoop,
    IndexNestedLoop,
    HashJoin,
    FactorizedAccumulate,
    ASPJoin,
    Flatten,
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
    NodeIdSeek {
        variable: String,
    },
    VectorSearch {
        variable: String,
        index: String,
    },
    RelationshipIdSeek {
        variable: String,
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
    OrderedTopK,
    Skip,
    Limit,
    Aggregate,
    Distinct,
    Optional,
    Cartesian,
    NestedLoop,
    IndexNestedLoop,
    HashJoin,
    FactorizedAccumulate,
    ASPJoin,
    Flatten,
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

impl PhysicalOperator {
    pub(crate) fn profile_name(&self) -> &'static str {
        match self {
            Self::NodeScan { .. } => "NodeScan",
            Self::LabelIndexScan { .. } => "LabelIndexScan",
            Self::IndexSeek { .. } => "IndexSeek",
            Self::NodeIdSeek { .. } => "NodeIdSeek",
            Self::RelationshipIdSeek { .. } => "RelationshipIdSeek",
            Self::VectorSearch { .. } => "VectorSearch",
            Self::RelationshipScan { .. } => "RelationshipScan",
            Self::AdjacencySeek { .. } => "AdjacencySeek",
            Self::Filter => "Filter",
            Self::Project => "Project",
            Self::ExternalSort => "ExternalSort",
            Self::OrderedTopK => "OrderedTopK",
            Self::Skip => "Skip",
            Self::Limit => "Limit",
            Self::Aggregate => "Aggregate",
            Self::Distinct => "Distinct",
            Self::Optional => "Optional",
            Self::Cartesian => "Cartesian",
            Self::NestedLoop => "NestedLoop",
            Self::IndexNestedLoop => "IndexNestedLoop",
            Self::HashJoin => "HashJoin",
            Self::FactorizedAccumulate => "FactorizedAccumulate",
            Self::ASPJoin => "ASPJoin",
            Self::Flatten => "Flatten",
            Self::Let => "Let",
            Self::Unwind => "Unwind",
            Self::Union { .. } => "Union",
            Self::Subquery => "Subquery",
            Self::When => "When",
            Self::Next => "Next",
            Self::Eager => "Eager",
            Self::Mutation { .. } => "Mutation",
            Self::Schema { .. } => "Schema",
            Self::Commit => "Commit",
        }
    }

    pub(crate) fn is_storage_access(&self) -> bool {
        matches!(
            self,
            Self::NodeScan { .. }
                | Self::LabelIndexScan { .. }
                | Self::IndexSeek { .. }
                | Self::NodeIdSeek { .. }
                | Self::RelationshipIdSeek { .. }
                | Self::VectorSearch { .. }
                | Self::RelationshipScan { .. }
                | Self::AdjacencySeek { .. }
        )
    }
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
    pub id_seek: Option<i64>,
    pub impossible: bool,
}
#[derive(Debug, Clone)]
pub(crate) struct RelationshipSpec {
    pub variable: Option<String>,
    pub type_id: Option<RelationshipTypeId>,
    pub type_name: Option<String>,
    pub direction: Direction,
    pub index_seek: Option<super::schema::StandardIndexSeek>,
    pub id_seek: Option<i64>,
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
    pub clause_start: usize,
    pub optional: bool,
    pub parts: Vec<PatternPart>,
    pub predicate: Option<Expr>,
    pub early_predicates: Vec<Vec<Expr>>,
    pub join: Option<std::sync::Arc<join::JoinPlan>>,
}

/// Without an incoming entity type proof, narrowing can skip the original
/// pattern's binding error. Keep the normal Label/Type producers and evaluate
/// the pattern before its WHERE, including on empty and OPTIONAL inputs.
/// Callers may select WHERE-based access only when this returns true; runtime
/// builders also apply it after choosing any previously prepared description.
pub(crate) fn constrain_match_scope(step: &mut MatchStep, incoming: &[String]) -> bool {
    if incoming.is_empty() || step.parts.is_empty() {
        return true;
    }
    let count = incoming.len().saturating_mul(step.parts.len());
    let unproven = count > budget::MAX_STATES || {
        // Inspect lengths before comparing names: count bounds alone cannot
        // bound many equal-length identifiers with a long common prefix.
        let incoming_bytes = incoming
            .iter()
            .fold(0_usize, |bytes, name| bytes.saturating_add(name.len()));
        let pattern_bytes = step
            .parts
            .iter()
            .flat_map(|part| {
                [
                    part.start.variable.as_deref(),
                    part.end.as_ref().and_then(|node| node.variable.as_deref()),
                    part.relationship
                        .as_ref()
                        .and_then(|rel| rel.variable.as_deref()),
                    part.path_variable.as_deref(),
                ]
            })
            .flatten()
            .fold(0_usize, |bytes, name| bytes.saturating_add(name.len()));
        let comparison_bytes = pattern_bytes
            .saturating_mul(incoming.len())
            .min(incoming_bytes.saturating_mul(step.parts.len().saturating_mul(4)));
        incoming_bytes.saturating_add(pattern_bytes) > budget::MAX_BYTES
            || comparison_bytes > budget::MAX_BYTES
            || incoming.iter().any(|name| {
                step.parts.iter().any(|part| {
                    part.start.variable.as_ref() == Some(name)
                        || part.end.as_ref().and_then(|node| node.variable.as_ref()) == Some(name)
                        || part
                            .relationship
                            .as_ref()
                            .and_then(|rel| rel.variable.as_ref())
                            == Some(name)
                        || part.path_variable.as_ref() == Some(name)
                })
            })
    };
    if unproven {
        step.early_predicates.clear();
        step.join = None;
        for part in &mut step.parts {
            part.start.id_seek = None;
            part.start.index_seek = None;
            if let Some(relationship) = &mut part.relationship {
                relationship.id_seek = None;
                relationship.index_seek = None;
            }
        }
    }
    !unproven
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

#[derive(Debug)]
pub struct PreparedQuery {
    pub resources: super::QueryResources,
    pub(crate) metrics: super::stream::QueryMetrics,
    pub(crate) access: AccessPolicy,
    pub(crate) commit: HashId,
    pub(crate) candidate: Option<super::version::CandidateContext>,
    pub(crate) resolved_state: Option<storage::ResolvedSnapshotState>,
    pub(crate) graph_view: ResolvedGraphView,
    pub(crate) matches: Vec<MatchStep>,
    pub(crate) profile_sites: Vec<ProfileBinding>,
    pub(crate) projections: Vec<Projection>,
    pub(crate) order: Vec<OrderItem>,
    pub(crate) ordered_topk: Option<super::schema::OrderedNumericIndexRead>,
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
    // Covers parameters and selected seeks; keep it until those fields drop.
    pub(crate) params_memory: Option<super::MemoryReservation>,
}

impl PreparedQuery {
    pub fn access_policy(&self) -> &AccessPolicy {
        &self.access
    }

    pub fn requires_transaction_boundary(&self) -> bool {
        self.program
            .as_ref()
            .is_some_and(|program| program.transaction_options.is_some())
    }

    pub fn has_external_io(&self) -> bool {
        self.program
            .as_ref()
            .is_some_and(|program| program.effects.has_external_io())
    }

    #[doc(hidden)]
    pub fn has_semantic_maintenance(&self) -> bool {
        self.program
            .as_ref()
            .is_some_and(|program| program.effects.semantic_maintenance)
    }
}

struct PrepareContext {
    access: AccessPolicy,
    commit: HashId,
    candidate: Option<super::version::CandidateContext>,
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
    let resources = super::QueryResources::new(options.memory_limit_bytes);
    prepare_with_resources(connection, query, params, options, resources)
}

/// Prepare an execution using a resource state shared with its SQL adapter.
pub fn prepare_with_resources(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
    resources: super::QueryResources,
) -> QueryResult<PreparedQuery> {
    prepare_with_resources_and_interrupt(connection, query, params, options, resources, &|| false)
}

/// Prepare using the same cancellation callback as execution advancement.
pub fn prepare_with_resources_and_interrupt(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
    resources: super::QueryResources,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<PreparedQuery> {
    let control = super::ExecutionControl::new(&resources, is_interrupted);
    control.check()?;
    let mut bytes = super::resources::checked_mul(
        super::resources::map_entry_bytes::<Value>()?,
        params.len() as u64,
    )?;
    for (name, value) in &params {
        bytes = super::resources::checked_add(bytes, name.capacity() as u64)?;
        bytes = super::resources::checked_add(
            bytes,
            super::resources::value_bytes_controlled(value, &control)?,
        )?;
    }
    let mut params_memory = resources.reserve(bytes)?;
    let mut prepared = prepare_inner(connection, query, params, options, &control)?;
    prepared.metrics = super::stream::QueryMetrics::for_execution_with_resources(
        prepared.mode,
        &prepared.physical,
        &resources,
    )?;
    prepared.install_match_profile_sites()?;
    prepared.resources = resources;
    if let Some(memory) = prepared.params_memory.take() {
        params_memory.merge(memory)?;
    }
    prepared.params_memory = Some(params_memory);
    Ok(prepared)
}

fn prepare_inner(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
    control: &super::ExecutionControl<'_>,
) -> QueryResult<PreparedQuery> {
    let ast = cypher::parse(query)?;
    cypher::analyze(&ast, query)?;
    validate_parameters(&ast.root, &params)?;
    super::safety::validate(&ast, &options)?;
    let context = prepare_context(connection, &ast, params, &options, control)?;
    if let Some(schema) = super::schema::prepare_schema(
        connection,
        context.commit,
        &ast,
        query,
        &context.params,
        &options,
        control,
    )? {
        return prepare_schema_query(context, schema);
    }
    if super::completeness::requires_program(&ast.root) {
        let program = super::completeness::prepare_program(connection, &ast, query, &options)?;
        return plan_program_query(connection, context, program, control);
    }
    reject_query_composition(&ast.root)?;
    let single = single_query(&ast.root)?;
    if contains_mutation(single) {
        reject_candidate_write(&context)?;
        return prepare_write_query(connection, single, query, &options, context, control);
    }
    prepare_read_query(connection, single, query, context, control)
}

fn plan_program_query(
    connection: &Connection,
    context: PrepareContext,
    mut program: super::completeness::PreparedProgram,
    control: &super::ExecutionControl<'_>,
) -> QueryResult<PreparedQuery> {
    if !program.root.descendants().any(|node| {
        matches!(
            node.kind,
            AstKind::Clause(ClauseKind::Match | ClauseKind::OptionalMatch)
        )
    }) {
        return prepare_program_query(context, program);
    }
    let state = resolve_planning_state(connection, &context, control)?;
    let snapshot = storage::Snapshot::from_resolved_state(connection, &state);
    let memory =
        program.plan_fixed_matches(&snapshot, &context.graph_view, &context.params, control)?;
    let mut prepared = prepare_program_query(context, program)?;
    prepared.resolved_state = Some(state);
    prepared.params_memory = Some(memory);
    Ok(prepared)
}

fn prepare_context(
    connection: &Connection,
    ast: &QueryAst,
    params: BTreeMap<String, Value>,
    options: &ExecutionOptions,
    control: &super::ExecutionControl<'_>,
) -> QueryResult<PrepareContext> {
    let candidate = options
        .merge_session
        .as_ref()
        .map(|selector| super::version::resolve_candidate_context(connection, selector, control))
        .transpose()?;
    let commit = match candidate.as_ref() {
        Some(candidate) => candidate.base_commit,
        None => resolve_commit(connection, &options.snapshot)?,
    };
    let graph_view = ResolvedGraphView::resolve(connection, &options.graph_view)?;
    Ok(PrepareContext {
        access: options.access.clone(),
        commit,
        candidate,
        graph_view: graph_view.with_query_access(options.access.read.query.clone()),
        params,
        mode: ast.execution_mode,
    })
}

fn prepare_schema_query(
    context: PrepareContext,
    schema: PreparedSchema,
) -> QueryResult<PreparedQuery> {
    reject_candidate_write(&context)?;
    Ok(schema_query(
        context.access,
        context.commit,
        context.candidate,
        context.graph_view,
        context.params,
        context.mode,
        schema,
    ))
}

fn prepare_program_query(
    context: PrepareContext,
    program: PreparedProgram,
) -> QueryResult<PreparedQuery> {
    validate_candidate_program(&context, &program)?;
    Ok(program_query(
        context.access,
        context.commit,
        context.candidate,
        context.graph_view,
        context.params,
        context.mode,
        program,
    ))
}

fn reject_candidate_write(context: &PrepareContext) -> QueryResult<()> {
    if context.candidate.is_some() {
        return Err(QueryError::read_only_snapshot(
            "Merge Session candidate context is read-only",
        ));
    }
    Ok(())
}

fn validate_candidate_program(
    context: &PrepareContext,
    program: &PreparedProgram,
) -> QueryResult<()> {
    if context.candidate.is_none() {
        return Ok(());
    }
    let owns_transaction = program.version_operation
        || program.transaction_options.is_some()
        || program.effects.load_csv
        || program.effects.semantic_maintenance;
    if owns_transaction {
        return Err(QueryError::transaction_boundary_required(
            "Merge Session candidate context cannot execute Version Procedures, external I/O, or transaction-owning Cypher",
        ));
    }
    if program.writes {
        return Err(QueryError::read_only_snapshot(
            "Merge Session candidate context is read-only",
        ));
    }
    Ok(())
}

fn prepare_write_query(
    connection: &Connection,
    single: &AstNode,
    query: &str,
    options: &ExecutionOptions,
    context: PrepareContext,
    control: &super::ExecutionControl<'_>,
) -> QueryResult<PreparedQuery> {
    let snapshot =
        super::graph::resolve_snapshot_with_control(connection, context.commit, control)?;
    let (write, memory) = super::mutation::prepare_write(
        &snapshot,
        single,
        query,
        &context.params,
        options,
        control,
    )?;
    let columns = if context.mode == ExecutionMode::Explain {
        vec!["plan".to_owned()]
    } else {
        write.columns.clone()
    };
    let logical = write.logical.clone();
    let physical = build_physical(&logical, &write.matches_for_explain, false);
    Ok(PreparedQuery {
        resources: super::QueryResources::default(),
        metrics: super::stream::QueryMetrics::default(),
        params_memory: Some(memory),
        access: context.access,
        commit: context.commit,
        candidate: context.candidate,
        resolved_state: Some(snapshot.resolved_state()),
        graph_view: context.graph_view,
        matches: Vec::new(),
        profile_sites: Vec::new(),
        projections: Vec::new(),
        order: Vec::new(),
        ordered_topk: None,
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

fn resolve_planning_state(
    connection: &Connection,
    context: &PrepareContext,
    control: &super::ExecutionControl<'_>,
) -> QueryResult<storage::ResolvedSnapshotState> {
    let observer = super::graph::snapshot_observer(control.resources());
    let check = || control.check().map_err(QueryError::into_storage_error);
    Ok(match context.candidate.as_ref() {
        Some(candidate) => storage::Snapshot::resolve_query_state_with_layer_and_schema_observed(
            connection,
            context.commit,
            &candidate.layer,
            candidate.schema.clone(),
            observer,
            &check,
        )?,
        None => storage::Snapshot::resolve_query_state_observed(
            connection,
            context.commit,
            observer,
            &check,
        )?,
    })
}

fn program_query(
    access: AccessPolicy,
    commit: HashId,
    candidate: Option<super::version::CandidateContext>,
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
    let logical = program.logical.clone();
    let physical = program.physical.clone();
    let mut prepared = empty_prepared_query(
        access, commit, candidate, graph_view, params, mode, logical, physical, columns,
    );
    prepared.program = Some(program);
    prepared
}

fn schema_query(
    access: AccessPolicy,
    commit: HashId,
    candidate: Option<super::version::CandidateContext>,
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
    let columns = if mode == ExecutionMode::Explain {
        vec!["plan".to_owned()]
    } else {
        Vec::new()
    };
    let mut prepared = empty_prepared_query(
        access, commit, candidate, graph_view, params, mode, logical, physical, columns,
    );
    prepared.schema = Some(schema);
    prepared
}

#[allow(
    clippy::too_many_arguments,
    reason = "PreparedQuery construction keeps its owned execution context explicit"
)]
fn empty_prepared_query(
    access: AccessPolicy,
    commit: HashId,
    candidate: Option<super::version::CandidateContext>,
    graph_view: ResolvedGraphView,
    params: BTreeMap<String, Value>,
    mode: ExecutionMode,
    logical: LogicalPlan,
    physical: PhysicalPlan,
    columns: Vec<String>,
) -> PreparedQuery {
    PreparedQuery {
        resources: super::QueryResources::default(),
        metrics: super::stream::QueryMetrics::default(),
        params_memory: None,
        access,
        commit,
        candidate,
        resolved_state: None,
        graph_view,
        matches: Vec::new(),
        profile_sites: Vec::new(),
        projections: Vec::new(),
        order: Vec::new(),
        ordered_topk: None,
        skip: 0,
        limit: None,
        distinct: false,
        aggregate: false,
        params,
        mode,
        write: None,
        schema: None,
        program: None,
        logical,
        physical,
        statistics: PlannerStatistics::default(),
        columns,
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

fn node_needs_scan_statistics(node: &NodeSpec) -> bool {
    node.id_seek.is_none() && node.index_seek.is_none() && !node.impossible && node.labels.len() > 1
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
    let mut step = MatchStep {
        clause_start: clause.span.start,
        optional,
        parts,
        predicate,
        early_predicates: Vec::new(),
        join: None,
    };
    predicate::route_safe_predicates(&mut step);
    Ok(step)
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
    let path_variable = part.path_assignment_variable().map(str::to_owned);
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
        .filter_map(|child| child.text.as_deref())
        .map(cypher::unescape_identifier)
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
        id_seek: None,
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
        .filter_map(|child| child.text.as_deref())
        .map(cypher::unescape_identifier)
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
        id_seek: None,
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
                .surface_projection_alias()
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
