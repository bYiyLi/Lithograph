//! Pull-based node intersections followed by complete relationship identity bags.

use std::collections::BTreeMap;
use std::sync::Arc;

use crate::query::expression::{self, BindingRow, BindingValue};
use crate::query::graph::ResolvedGraphView;
use crate::query::plan::{MatchStep, MultiwayPlan, PatternPart};
use crate::query::resources::{allocation_bytes, checked_add, checked_mul, map_entry_bytes};
use crate::query::{ExecutionControl, MemoryReservation, QueryError, QueryResult};
use crate::storage::Snapshot;

use super::{MatchReadBindings, MultiwayRuntimeMetrics, QueryMetrics, StepCursor, node_matches};

mod adjacency;
mod levels;
mod relationships;
use levels::NodeIntersection;
use relationships::RelationshipProduct;

#[derive(Debug)]
pub(crate) struct MultiwayProducer {
    step: MatchStep,
    plan: Arc<MultiwayPlan>,
    base: BindingRow,
    root: Box<StepCursor>,
    root_bindings: MatchReadBindings,
    root_metrics: QueryMetrics,
    nodes: Vec<i64>,
    levels: Vec<NodeIntersection>,
    depth: usize,
    done: bool,
    relationships: Option<RelationshipProduct>,
    stats: MultiwayRuntimeMetrics,
    published: MultiwayRuntimeMetrics,
    memory: MemoryReservation,
    workspace: MemoryReservation,
    output_memory: MemoryReservation,
}

impl MultiwayProducer {
    pub(crate) fn new(
        step: &MatchStep,
        mut base: BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Self> {
        let plan = step
            .join
            .as_ref()
            .and_then(|join| join.multiway.as_ref())
            .ok_or_else(|| QueryError::internal("multiway cursor lacks its plan"))?;
        let root = plan
            .nodes
            .first()
            .ok_or_else(|| QueryError::internal("multiway cursor has no node variables"))?;
        base.used_relationships.clear();
        let memory = control.reserve(producer_memory_bytes(&base, plan, control)?)?;
        let workspace = control.reserve(adjacency::scratch_workspace_bytes()?)?;
        let root_step = MatchStep {
            clause_start: step.clause_start,
            optional: false,
            parts: vec![PatternPart {
                path_variable: None,
                start: root.spec.clone(),
                relationship: None,
                end: None,
            }],
            predicate: None,
            early_predicates: Vec::new(),
            join: None,
        };
        let root = Box::new(StepCursor::new(&root_step, BindingRow::default(), control)?);
        Ok(Self {
            step: step.clone(),
            plan: Arc::clone(plan),
            base,
            root,
            root_bindings: MatchReadBindings::default(),
            root_metrics: QueryMetrics::default(),
            nodes: vec![0; plan.nodes.len()],
            levels: Vec::with_capacity(plan.nodes.len().saturating_sub(1)),
            depth: 0,
            done: false,
            relationships: None,
            stats: MultiwayRuntimeMetrics::default(),
            published: MultiwayRuntimeMetrics::default(),
            memory,
            workspace,
            output_memory: control.reserve(0)?,
        })
    }

    pub(crate) fn next_row(
        &mut self,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        control.check()?;
        if self.plan.snapshot_identity != scope.0.cache_identity() {
            return Err(QueryError::internal(
                "multiway plan belongs to another Snapshot",
            ));
        }
        // Grace parks this claim only between pulls, while encoding an
        // already-yielded row. Restore it before any adjacency load or state
        // advance; a failed reservation propagates without consuming input.
        self.workspace
            .resize(adjacency::scratch_workspace_bytes()?)?;
        let slot = metrics.multiway_slot(self.plan.clause_start);
        let previous = metrics.activate_profile_slot(slot);
        let result = self.advance(scope, metrics, control);
        self.record_memory();
        metrics.add_multiway_profile(slot, self.stats.take_delta(&mut self.published));
        if matches!(&result, Ok(Some(_))) {
            metrics.record_profile_rows(slot, 1);
        }
        metrics.restore_access_operator(previous);
        result
    }

    pub(crate) fn park_workspace(&mut self) -> QueryResult<()> {
        // Called only after a row was yielded and the consumer committed to
        // Grace. Snapshot's synchronous merge scratch has already been freed.
        self.workspace.resize(0)
    }

    fn advance(
        &mut self,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<Option<BindingRow>> {
        while !self.done {
            control.work(1)?;
            if let Some(product) = self.relationships.as_mut() {
                let available = product.next(scope, metrics, &mut self.stats, control)?;
                self.record_memory();
                if available {
                    return self.output_row(control).map(Some);
                }
                self.relationships = None;
            }
            if !self.next_nodes(scope, metrics, control)? {
                self.done = true;
                break;
            }
            self.stats.node_assignments = self.stats.node_assignments.saturating_add(1);
            self.relationships = Some(RelationshipProduct::new(
                &self.plan,
                &self.nodes,
                &mut self.stats,
                control,
            )?);
            self.record_memory();
        }
        Ok(None)
    }

    fn next_nodes(
        &mut self,
        scope: (&Snapshot<'_>, &ResolvedGraphView),
        metrics: &mut QueryMetrics,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<bool> {
        loop {
            control.work(1)?;
            if self.depth == 0 {
                let previous_hits = self.root_metrics.db_hits;
                // This internal step contains only frozen node access; the
                // original parameterized WHERE stays at the MATCH boundary.
                let result = self.root.next_row(
                    scope.0,
                    scope.1,
                    &BTreeMap::new(),
                    &mut self.root_metrics,
                    &mut self.root_bindings,
                    control,
                );
                metrics.record_db_hits(self.root_metrics.db_hits.saturating_sub(previous_hits));
                let Some(row) = result? else {
                    return Ok(false);
                };
                let Some(BindingValue::Node(id)) = row.values.get(&self.plan.nodes[0].variable)
                else {
                    return Err(QueryError::internal("multiway root did not bind its Node"));
                };
                self.nodes[0] = *id;
                self.depth = 1;
                if self.depth == self.nodes.len() {
                    self.depth = 0;
                    return Ok(true);
                }
            }
            if self.levels.len() < self.depth {
                self.levels.push(NodeIntersection::new(
                    &self.plan,
                    self.depth,
                    &self.nodes,
                    &mut self.stats,
                    control,
                )?);
            }
            let candidate =
                self.levels[self.depth - 1].next(scope.0, metrics, &mut self.stats, control)?;
            self.record_memory();
            let Some(id) = candidate else {
                self.levels.pop();
                self.depth -= 1;
                continue;
            };
            self.stats.candidate_nodes = self.stats.candidate_nodes.saturating_add(1);
            let spec = &self.plan.nodes[self.depth].spec;
            if spec.impossible
                || spec.id_seek.is_some_and(|wanted| wanted != id)
                || !node_matches(scope.0, scope.1, spec, id, metrics, control)?
            {
                continue;
            }
            self.nodes[self.depth] = id;
            if self.depth + 1 == self.nodes.len() {
                return Ok(true);
            }
            self.depth += 1;
        }
    }

    fn output_row(&mut self, control: &ExecutionControl<'_>) -> QueryResult<BindingRow> {
        self.output_memory.resize(self.output_bytes(control)?)?;
        let mut row =
            expression::clone_binding_row_unreserved_controlled(&self.base, Some(control))?;
        for (node, id) in self.plan.nodes.iter().zip(&self.nodes) {
            control.work(1)?;
            row.insert(node.variable.clone(), BindingValue::Node(*id));
        }
        let product = self
            .relationships
            .as_ref()
            .ok_or_else(|| QueryError::internal("multiway output lost its relationship product"))?;
        for (edge, relationship) in self.plan.edges.iter().zip(product.selected()) {
            control.work(1)?;
            let spec = self.step.parts[edge.part]
                .relationship
                .as_ref()
                .ok_or_else(|| QueryError::internal("multiway edge lost its relationship spec"))?;
            if !row.bind_relationship(spec.variable.as_deref(), relationship)? {
                return Err(QueryError::internal(
                    "multiway identity product bound inconsistent values",
                ));
            }
            row.used_relationships.insert(relationship.id);
        }
        self.record_memory();
        Ok(row)
    }

    fn output_bytes(&self, control: &ExecutionControl<'_>) -> QueryResult<u64> {
        output_shape_bytes(
            expression::binding_row_bytes_controlled(&self.base, Some(control))?,
            self.base.order.len(),
            &self.step,
            &self.plan,
            control,
        )
    }

    pub(crate) fn split_output_memory(
        &mut self,
        row: &BindingRow,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<MemoryReservation> {
        self.output_memory
            .split(expression::binding_row_bytes_controlled(
                row,
                Some(control),
            )?)
    }

    fn record_memory(&mut self) {
        let levels = self
            .levels
            .iter()
            .map(NodeIntersection::retained_bytes)
            .sum::<u64>();
        let product = self
            .relationships
            .as_ref()
            .map_or(0, RelationshipProduct::retained_bytes);
        let bytes = self
            .memory
            .bytes()
            .saturating_add(self.output_memory.bytes())
            .saturating_add(self.workspace.bytes())
            .saturating_add(self.root.node_scan_memory_bytes())
            .saturating_add(levels)
            .saturating_add(product);
        self.stats.memory_peak_bytes = self.stats.memory_peak_bytes.max(bytes);
    }
}

fn output_shape_bytes(
    mut bytes: u64,
    mut count: usize,
    step: &MatchStep,
    plan: &MultiwayPlan,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    for name in plan
        .nodes
        .iter()
        .map(|node| &node.variable)
        .chain(plan.edges.iter().filter_map(|edge| {
            step.parts[edge.part]
                .relationship
                .as_ref()?
                .variable
                .as_ref()
        }))
    {
        control.work(1)?;
        bytes = checked_add(bytes, map_entry_bytes::<BindingValue>()?)?;
        bytes = checked_add(bytes, checked_mul(name.len() as u64, 2)?)?;
        count += 1;
    }
    bytes = checked_add(
        bytes,
        allocation_bytes::<String>(count.next_power_of_two().max(4))?,
    )?;
    checked_add(
        bytes,
        checked_mul(map_entry_bytes::<i64>()?, plan.edges.len() as u64)?,
    )
}

fn producer_memory_bytes(
    base: &BindingRow,
    plan: &MultiwayPlan,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let bytes = checked_add(
        std::mem::size_of::<MultiwayProducer>() as u64,
        expression::binding_row_bytes_controlled(base, Some(control))?,
    )?;
    let bytes = checked_add(bytes, allocation_bytes::<i64>(plan.nodes.len())?)?;
    let bytes = checked_add(
        bytes,
        allocation_bytes::<NodeIntersection>(plan.nodes.len().saturating_sub(1))?,
    )?;
    checked_add(bytes, std::mem::size_of::<StepCursor>() as u64)
}

/// Additional bounded multiway workspace used by the existing cost model.
/// Root access itself keeps its already-priced original node access path.
pub(crate) fn planning_memory_bytes(nodes: usize, edges: usize) -> QueryResult<u64> {
    let lists = edges.saturating_mul(2);
    let mut bytes = std::mem::size_of::<MultiwayProducer>() as u64;
    bytes = checked_add(bytes, allocation_bytes::<i64>(nodes.saturating_add(edges))?)?;
    bytes = checked_add(
        bytes,
        allocation_bytes::<NodeIntersection>(nodes.saturating_sub(1))?,
    )?;
    bytes = checked_add(bytes, allocation_bytes::<adjacency::AdjacentList>(lists)?)?;
    bytes = checked_add(
        bytes,
        allocation_bytes::<crate::storage::RelationshipRecord>(edges)?,
    )?;
    bytes = checked_add(
        bytes,
        checked_mul(adjacency::page_retained_bytes()?, lists as u64)?,
    )?;
    checked_add(bytes, adjacency::scratch_workspace_bytes()?)
}

/// One synchronous adjacency merge workspace, parked only while an already
/// selected Grace consumer serializes a yielded component row.
pub(crate) fn pull_workspace_bytes() -> QueryResult<u64> {
    adjacency::scratch_workspace_bytes()
}

/// Complete component owner bound for a proved empty base, including its root
/// cursor and both output leases. The containing stack and shared Hash/index
/// slots are added separately by the full prefix bound below.
pub(crate) fn planning_owner_memory_bytes(
    step: &MatchStep,
    plan: &MultiwayPlan,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    if plan.nodes.is_empty() {
        return Err(QueryError::internal(
            "multiway memory bound requires its root Node",
        ));
    }
    // Pricing may choose a different root order after this fixed workspace is
    // prepared. Exactly one root cursor is live, so cover the widest eligible
    // root without adding together mutually exclusive cursor allocations.
    let mut root = 0;
    for node in &plan.nodes {
        control.work(1)?;
        root = root.max(StepCursor::planning_node_memory_bytes(&node.spec, control)?);
    }
    let empty = allocation_bytes::<BindingRow>(1)?;
    let workspace = planning_memory_bytes(plan.nodes.len(), plan.edges.len())?;
    let bytes = checked_add(workspace, empty)?;
    let bytes = checked_add(bytes, root)?;
    checked_add(bytes, output_shape_bytes(empty, 0, step, plan, control)?)
}

/// Full prefix resident reservation, including the shared original stack and
/// its possible tail Hash/index slots. Only a proved empty base is covered.
pub(crate) fn planning_prefix_memory_bytes(
    step: &MatchStep,
    plan: &MultiwayPlan,
    binary: &crate::query::plan::join::JoinPlan,
    control: &ExecutionControl<'_>,
) -> QueryResult<u64> {
    let owner = planning_owner_memory_bytes(step, plan, control)?;
    let stack = StepCursor::planning_outer_memory_bytes(step, control)?;
    let bindings = checked_add(
        MatchReadBindings::planning_memory_bytes(binary)?,
        std::mem::size_of::<MatchReadBindings>() as u64,
    )?;
    checked_add(checked_add(owner, stack)?, bindings)
}

/// The first Grace tail can park merge scratch while encoding a yielded row,
/// then drain the entire component before loading leaf tables. Outer stack and
/// shared bindings survive both boundaries. Other prefixes need their own
/// lifetime proof and must not use these reductions.
pub(crate) fn planning_prefix_phase_memory(
    step: &MatchStep,
    plan: &MultiwayPlan,
    control: &ExecutionControl<'_>,
    resident: u64,
) -> QueryResult<(u64, u64)> {
    let owner = planning_owner_memory_bytes(step, plan, control)?;
    let yielded = resident
        .checked_sub(pull_workspace_bytes()?)
        .ok_or_else(|| {
            QueryError::internal("Grace yielded prefix memory omits its merge workspace")
        })?;
    let drained = resident.checked_sub(owner).ok_or_else(|| {
        QueryError::internal("Grace drained prefix memory omits its component owner")
    })?;
    Ok((yielded, drained))
}
