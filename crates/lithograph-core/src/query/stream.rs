use std::time::Instant;

use rusqlite::Connection;

use crate::cypher::{ExecutionMode, Value};
use crate::storage::{RelationshipRecord, Snapshot};

use super::expression::{self, BindingRow, BindingValue};
use super::graph::ResolvedGraphView;
use super::plan::{Direction, MatchStep, NodeSpec, PatternPart, PreparedQuery, RelationshipSpec};
use super::spill::{
    DistinctSpill, SortSpill, SpillOutput, drop_temp_table, open_spill_connection, read_output_row,
};
use super::{QueryError, QueryResult};

const PIPELINE_BATCH: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryMetrics {
    pub rows: u64,
    pub db_hits: u64,
    pub elapsed_micros: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySummary {
    pub commit: String,
    pub metrics: QueryMetrics,
}

#[derive(Debug, Clone, PartialEq)]
pub struct QueryBatch {
    pub rows: Vec<Vec<Value>>,
    pub done: bool,
    pub summary: Option<QuerySummary>,
}

#[derive(Debug)]
pub struct QueryCursor {
    prepared: PreparedQuery,
    pipeline: MatchPipeline,
    metrics: QueryMetrics,
    started: Instant,
    skipped: usize,
    emitted: usize,
    explain_emitted: bool,
    aggregate_state: Option<Vec<u64>>,
    barrier: BarrierState,
    spill_connection: Option<Connection>,
    finished: bool,
}

#[derive(Debug)]
enum BarrierState {
    Direct,
    Pending,
    Ready(SpillOutput),
}

#[derive(Debug)]
struct MatchPipeline {
    steps: Vec<MatchStep>,
    stack: Vec<StepCursor>,
    initialized: bool,
    empty_seed_emitted: bool,
}

impl MatchPipeline {
    fn new(steps: Vec<MatchStep>) -> Self {
        Self {
            steps,
            stack: Vec::new(),
            initialized: false,
            empty_seed_emitted: false,
        }
    }

    fn next_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<BindingRow>> {
        if self.steps.is_empty() {
            if self.empty_seed_emitted {
                return Ok(None);
            }
            self.empty_seed_emitted = true;
            return Ok(Some(BindingRow::default()));
        }
        if !self.initialized {
            self.stack.push(StepCursor::new(
                self.steps[0].clone(),
                BindingRow::default(),
            ));
            self.initialized = true;
        }
        loop {
            let depth = self.stack.len();
            let Some(frame) = self.stack.last_mut() else {
                return Ok(None);
            };
            if let Some(row) = frame.next_row(snapshot, graph_view, params, metrics)? {
                if depth == self.steps.len() {
                    return Ok(Some(row));
                }
                self.stack
                    .push(StepCursor::new(self.steps[depth].clone(), row));
                continue;
            }
            self.stack.pop();
        }
    }
}

#[derive(Debug)]
struct StepCursor {
    step: MatchStep,
    base: BindingRow,
    stack: Vec<PartCursor>,
    initialized: bool,
    matched: bool,
    optional_emitted: bool,
}

impl StepCursor {
    fn new(step: MatchStep, mut base: BindingRow) -> Self {
        // Relationship uniqueness is scoped to one MATCH graph pattern. A
        // later MATCH clause may legally traverse a Relationship used by an
        // earlier MATCH while still inheriting its variable bindings.
        base.used_relationships.clear();
        Self {
            step,
            base,
            stack: Vec::new(),
            initialized: false,
            matched: false,
            optional_emitted: false,
        }
    }

    fn next_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        params: &std::collections::BTreeMap<String, Value>,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<BindingRow>> {
        if !self.initialized {
            if let Some(part) = self.step.parts.first() {
                self.stack
                    .push(PartCursor::new(part.clone(), self.base.clone()));
            }
            self.initialized = true;
        }
        loop {
            let depth = self.stack.len();
            if depth == 0 {
                return self.optional_or_done();
            }
            let Some(frame) = self.stack.last_mut() else {
                return self.optional_or_done();
            };
            if let Some(row) = frame.next_row(snapshot, graph_view, metrics)? {
                if depth < self.step.parts.len() {
                    self.stack
                        .push(PartCursor::new(self.step.parts[depth].clone(), row));
                    continue;
                }
                let passes = match &self.step.predicate {
                    Some(predicate) => expression::predicate(expression::evaluate(
                        predicate, snapshot, &row, params,
                    )?)?,
                    None => true,
                };
                if passes {
                    self.matched = true;
                    return Ok(Some(row));
                }
                continue;
            }
            self.stack.pop();
        }
    }

    fn optional_or_done(&mut self) -> QueryResult<Option<BindingRow>> {
        if self.step.optional && !self.matched && !self.optional_emitted {
            self.optional_emitted = true;
            let mut row = self.base.clone();
            for part in &self.step.parts {
                set_optional_null(&mut row, part.path_variable.as_deref());
                set_optional_null(&mut row, part.start.variable.as_deref());
                if let Some(rel) = &part.relationship {
                    set_optional_null(&mut row, rel.variable.as_deref());
                }
                if let Some(end) = &part.end {
                    set_optional_null(&mut row, end.variable.as_deref());
                }
            }
            return Ok(Some(row));
        }
        Ok(None)
    }
}

fn set_optional_null(row: &mut BindingRow, variable: Option<&str>) {
    if let Some(variable) = variable {
        row.values
            .entry(variable.to_owned())
            .or_insert(BindingValue::Null);
    }
}

#[derive(Debug)]
struct PartCursor {
    part: PatternPart,
    base: BindingRow,
    start_after: i64,
    start_buffer: Vec<i64>,
    start_index: usize,
    start_done: bool,
    current_start: Option<i64>,
    relationships: Vec<RelationshipRecord>,
    relationship_index: usize,
    relationship_after: i64,
    relationship_done: bool,
    bound_start_emitted: bool,
}

impl PartCursor {
    fn new(part: PatternPart, base: BindingRow) -> Self {
        Self {
            part,
            base,
            start_after: 0,
            start_buffer: Vec::new(),
            start_index: 0,
            start_done: false,
            current_start: None,
            relationships: Vec::new(),
            relationship_index: 0,
            relationship_after: 0,
            relationship_done: false,
            bound_start_emitted: false,
        }
    }

    fn next_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<BindingRow>> {
        if self.part_is_impossible(graph_view) {
            return Ok(None);
        }
        if self.part.relationship.is_none() {
            let Some(id) = self.next_start(snapshot, graph_view, metrics)? else {
                return Ok(None);
            };
            let Some(row) = bind_node(self.base.clone(), self.part.start.variable.as_deref(), id)?
            else {
                return Ok(None);
            };
            return bind_path(
                row,
                self.part.path_variable.as_deref(),
                vec![id],
                Vec::new(),
            );
        }
        loop {
            if self.relationship_index < self.relationships.len() {
                let rel = self.relationships[self.relationship_index];
                self.relationship_index += 1;
                if let Some(row) = self.match_relationship(snapshot, graph_view, rel, metrics)? {
                    return Ok(Some(row));
                }
                continue;
            }
            if let Some(start) = self.current_start
                && !self.relationship_done
            {
                let page =
                    self.load_relationship_page(snapshot, start, self.relationship_after, metrics)?;
                self.relationships = page.items;
                self.relationship_index = 0;
                if let Some(after) = page.next_after {
                    self.relationship_after = after;
                } else {
                    self.relationship_done = true;
                }
                if !self.relationships.is_empty() || !self.relationship_done {
                    continue;
                }
            }
            let Some(start) = self.next_start(snapshot, graph_view, metrics)? else {
                return Ok(None);
            };
            self.current_start = Some(start);
            self.relationships.clear();
            self.relationship_index = 0;
            self.relationship_after = 0;
            self.relationship_done = false;
        }
    }

    fn part_is_impossible(&self, graph_view: &ResolvedGraphView) -> bool {
        graph_view.is_empty()
            || self.part.start.impossible
            || self.part.end.as_ref().is_some_and(|node| node.impossible)
            || self
                .part
                .relationship
                .as_ref()
                .is_some_and(|relationship| relationship.impossible)
    }

    fn next_start(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<i64>> {
        if let Some(variable) = self.part.start.variable.as_deref()
            && let Some(value) = self.base.values.get(variable)
        {
            if self.bound_start_emitted {
                return Ok(None);
            }
            self.bound_start_emitted = true;
            let BindingValue::Node(id) = value else {
                return Ok(None);
            };
            return if node_matches(snapshot, graph_view, &self.part.start, *id, metrics)? {
                Ok(Some(*id))
            } else {
                Ok(None)
            };
        }
        self.next_scanned_start(snapshot, graph_view, metrics)
    }

    fn next_scanned_start(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<i64>> {
        loop {
            while self.start_index < self.start_buffer.len() {
                let id = self.start_buffer[self.start_index];
                self.start_index += 1;
                if node_matches(snapshot, graph_view, &self.part.start, id, metrics)? {
                    return Ok(Some(id));
                }
            }
            if self.start_done {
                return Ok(None);
            }
            let scan_label = self
                .part
                .start
                .scan_label
                .or_else(|| graph_view.scan_label());
            let page = if let Some(label) = scan_label {
                snapshot.scan_label_after(label, self.start_after, PIPELINE_BATCH)?
            } else {
                snapshot.scan_nodes_after(self.start_after, PIPELINE_BATCH)?
            };
            metrics.db_hits = metrics.db_hits.saturating_add(page.items.len() as u64);
            self.start_buffer = page.items;
            self.start_index = 0;
            if let Some(after) = page.next_after {
                self.start_after = after;
            } else {
                self.start_done = true;
            }
        }
    }

    fn load_relationship_page(
        &self,
        snapshot: &Snapshot<'_>,
        start: i64,
        after: i64,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<crate::storage::ScanPage<RelationshipRecord>> {
        let Some(spec) = &self.part.relationship else {
            return Ok(crate::storage::ScanPage {
                items: Vec::new(),
                next_after: None,
            });
        };
        let page = match spec.direction {
            Direction::Outgoing => {
                snapshot.scan_outgoing_after(start, spec.type_id, after, PIPELINE_BATCH)?
            }
            Direction::Incoming => {
                snapshot.scan_incoming_after(start, spec.type_id, after, PIPELINE_BATCH)?
            }
            Direction::Undirected => {
                snapshot.scan_incident_after(start, spec.type_id, after, PIPELINE_BATCH)?
            }
        };
        metrics.db_hits = metrics.db_hits.saturating_add(page.items.len() as u64);
        Ok(page)
    }

    fn match_relationship(
        &self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        relationship: RelationshipRecord,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<BindingRow>> {
        let Some(start) = self.current_start else {
            return Ok(None);
        };
        let Some(spec) = &self.part.relationship else {
            return Ok(None);
        };
        if !graph_view.visible_relationship(snapshot, relationship)? {
            return Ok(None);
        }
        metrics.db_hits = metrics.db_hits.saturating_add(2);
        let end_id = match spec.direction {
            Direction::Outgoing => relationship.target,
            Direction::Incoming => relationship.source,
            Direction::Undirected if relationship.source == start => relationship.target,
            Direction::Undirected => relationship.source,
        };
        let Some(end_spec) = &self.part.end else {
            return Ok(None);
        };
        if !node_matches(snapshot, graph_view, end_spec, end_id, metrics)? {
            return Ok(None);
        }
        if self.base.used_relationships.contains(&relationship.id)
            && !relationship_already_bound(&self.base, spec, relationship.id)
        {
            return Ok(None);
        }
        let Some(row) = bind_node(
            self.base.clone(),
            self.part.start.variable.as_deref(),
            start,
        )?
        else {
            return Ok(None);
        };
        let Some(row) = bind_relationship(row, spec, relationship)? else {
            return Ok(None);
        };
        let Some(row) = bind_node(row, end_spec.variable.as_deref(), end_id)? else {
            return Ok(None);
        };
        let Some(mut row) = bind_path(
            row,
            self.part.path_variable.as_deref(),
            vec![start, end_id],
            vec![relationship],
        )?
        else {
            return Ok(None);
        };
        row.used_relationships.insert(relationship.id);
        Ok(Some(row))
    }
}

fn relationship_already_bound(row: &BindingRow, spec: &RelationshipSpec, id: i64) -> bool {
    spec.variable
        .as_deref()
        .and_then(|name| row.values.get(name))
        .is_some_and(|value| matches!(value, BindingValue::Relationship(rel) if rel.id == id))
}

fn node_matches(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    spec: &NodeSpec,
    node_id: i64,
    metrics: &mut QueryMetrics,
) -> QueryResult<bool> {
    if !graph_view.visible_node(snapshot, node_id)? {
        return Ok(false);
    }
    metrics.db_hits = metrics.db_hits.saturating_add(1);
    if spec.labels.is_empty() {
        return Ok(true);
    }
    let labels = snapshot.labels(node_id)?;
    metrics.db_hits = metrics.db_hits.saturating_add(1);
    Ok(spec
        .labels
        .iter()
        .all(|label| labels.binary_search(label).is_ok()))
}

fn bind_node(
    mut row: BindingRow,
    variable: Option<&str>,
    node_id: i64,
) -> QueryResult<Option<BindingRow>> {
    let Some(variable) = variable else {
        return Ok(Some(row));
    };
    match row.values.get(variable) {
        Some(BindingValue::Node(existing)) if *existing == node_id => Ok(Some(row)),
        Some(BindingValue::Node(_) | BindingValue::Null) => Ok(None),
        Some(_) => Err(QueryError::semantic(format!(
            "variable {variable} is not a Node binding"
        ))),
        None => {
            row.values
                .insert(variable.to_owned(), BindingValue::Node(node_id));
            Ok(Some(row))
        }
    }
}

fn bind_relationship(
    mut row: BindingRow,
    spec: &RelationshipSpec,
    relationship: RelationshipRecord,
) -> QueryResult<Option<BindingRow>> {
    let Some(variable) = spec.variable.as_deref() else {
        return Ok(Some(row));
    };
    match row.values.get(variable) {
        Some(BindingValue::Relationship(existing)) if existing.id == relationship.id => {
            Ok(Some(row))
        }
        Some(BindingValue::Relationship(_) | BindingValue::Null) => Ok(None),
        Some(_) => Err(QueryError::semantic(format!(
            "variable {variable} is not a Relationship binding"
        ))),
        None => {
            row.values.insert(
                variable.to_owned(),
                BindingValue::Relationship(relationship),
            );
            Ok(Some(row))
        }
    }
}

fn bind_path(
    mut row: BindingRow,
    variable: Option<&str>,
    nodes: Vec<i64>,
    relationships: Vec<RelationshipRecord>,
) -> QueryResult<Option<BindingRow>> {
    let Some(variable) = variable else {
        return Ok(Some(row));
    };
    match row.values.get(variable) {
        Some(BindingValue::Path {
            nodes: existing_nodes,
            relationships: existing_relationships,
        }) if *existing_nodes == nodes && *existing_relationships == relationships => Ok(Some(row)),
        Some(BindingValue::Path { .. } | BindingValue::Null) => Ok(None),
        Some(_) => Err(QueryError::semantic(format!(
            "variable {variable} is not a Path binding"
        ))),
        None => {
            row.values.insert(
                variable.to_owned(),
                BindingValue::Path {
                    nodes,
                    relationships,
                },
            );
            Ok(Some(row))
        }
    }
}

impl QueryCursor {
    pub fn new(prepared: PreparedQuery) -> Self {
        let barrier = if prepared.order.is_empty() && !prepared.distinct {
            BarrierState::Direct
        } else {
            BarrierState::Pending
        };
        Self {
            pipeline: MatchPipeline::new(prepared.matches.clone()),
            prepared,
            metrics: QueryMetrics::default(),
            started: Instant::now(),
            skipped: 0,
            emitted: 0,
            explain_emitted: false,
            aggregate_state: None,
            barrier,
            spill_connection: None,
            finished: false,
        }
    }

    pub fn columns(&self) -> &[String] {
        &self.prepared.columns
    }

    pub fn next_batch(
        &mut self,
        connection: &Connection,
        max_rows: usize,
    ) -> QueryResult<QueryBatch> {
        self.next_batch_with_interrupt(connection, max_rows, &|| false)
    }

    pub fn next_batch_with_interrupt(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        if self.finished {
            return Ok(QueryBatch {
                rows: Vec::new(),
                done: true,
                summary: None,
            });
        }
        check_interrupted(is_interrupted)?;
        let max_rows = max_rows.clamp(1, 4_096);
        if self.prepared.mode == ExecutionMode::Explain {
            return self.next_explain();
        }
        let snapshot = Snapshot::resolve(connection, self.prepared.commit)?;
        if self.prepared.aggregate {
            return self.next_aggregate(&snapshot, is_interrupted);
        }
        match self.barrier {
            BarrierState::Direct => self.next_direct(&snapshot, max_rows, is_interrupted),
            BarrierState::Pending => {
                self.build_barrier(connection, &snapshot, is_interrupted)?;
                self.next_spilled(max_rows, is_interrupted)
            }
            BarrierState::Ready(_) => self.next_spilled(max_rows, is_interrupted),
        }
    }

    fn next_explain(&mut self) -> QueryResult<QueryBatch> {
        if self.explain_emitted {
            return self.finish(Vec::new());
        }
        self.explain_emitted = true;
        self.metrics.rows = 1;
        self.finish(vec![vec![Value::String(self.prepared.physical.explain())]])
    }

    fn next_direct(
        &mut self,
        snapshot: &Snapshot<'_>,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            check_interrupted(is_interrupted)?;
            if self
                .prepared
                .limit
                .is_some_and(|limit| self.emitted >= limit)
            {
                return self.finish(rows);
            }
            let next = self.pipeline.next_row(
                snapshot,
                &self.prepared.graph_view,
                &self.prepared.params,
                &mut self.metrics,
            )?;
            let Some(binding) = next else {
                return self.finish(rows);
            };
            if self.skipped < self.prepared.skip {
                self.skipped += 1;
                continue;
            }
            rows.push(project_row(&self.prepared, snapshot, &binding)?);
            self.emitted += 1;
            self.metrics.rows = self.metrics.rows.saturating_add(1);
        }
        Ok(QueryBatch {
            rows,
            done: false,
            summary: None,
        })
    }

    fn next_aggregate(
        &mut self,
        snapshot: &Snapshot<'_>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        if self.aggregate_state.is_none() {
            let mut counts = vec![0_u64; self.prepared.projections.len()];
            loop {
                check_interrupted(is_interrupted)?;
                let Some(row) = self.pipeline.next_row(
                    snapshot,
                    &self.prepared.graph_view,
                    &self.prepared.params,
                    &mut self.metrics,
                )?
                else {
                    break;
                };
                for (index, projection) in self.prepared.projections.iter().enumerate() {
                    if expression::count_contributes(
                        &projection.expression,
                        snapshot,
                        &row,
                        &self.prepared.params,
                    )? {
                        counts[index] = counts[index].saturating_add(1);
                    }
                }
            }
            self.aggregate_state = Some(counts);
        }
        if self.prepared.skip > 0 || self.prepared.limit == Some(0) {
            return self.finish(Vec::new());
        }
        let counts = self.aggregate_state.take().unwrap_or_default();
        let row = counts
            .into_iter()
            .map(|value| Value::Integer(i64::try_from(value).unwrap_or(i64::MAX)))
            .collect();
        self.metrics.rows = 1;
        self.finish(vec![row])
    }

    fn build_barrier(
        &mut self,
        connection: &Connection,
        snapshot: &Snapshot<'_>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if self.prepared.order.is_empty() {
            return self.build_distinct_spill(snapshot, is_interrupted);
        }
        self.build_sort_spill(connection, snapshot, is_interrupted)
    }

    fn build_distinct_spill(
        &mut self,
        snapshot: &Snapshot<'_>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        let spill_connection = open_spill_connection()?;
        let mut spill = DistinctSpill::create(&spill_connection)?;
        let result =
            self.populate_distinct_spill(&spill_connection, snapshot, &mut spill, is_interrupted);
        if let Err(error) = result {
            let _ = spill.abort(&spill_connection);
            return Err(error);
        }
        self.barrier = BarrierState::Ready(spill.output());
        self.spill_connection = Some(spill_connection);
        Ok(())
    }

    fn populate_distinct_spill(
        &mut self,
        spill_connection: &Connection,
        snapshot: &Snapshot<'_>,
        spill: &mut DistinctSpill,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        while let Some((_, row)) = self.next_spill_row(snapshot, is_interrupted)? {
            spill.push(spill_connection, &row)?;
        }
        Ok(())
    }

    fn build_sort_spill(
        &mut self,
        _connection: &Connection,
        snapshot: &Snapshot<'_>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        let directions = self
            .prepared
            .order
            .iter()
            .map(|item| item.descending)
            .collect::<Vec<_>>();
        let spill_connection = open_spill_connection()?;
        let mut spill = SortSpill::create(&spill_connection, self.prepared.distinct, directions)?;
        let result =
            self.populate_sort_spill(&spill_connection, snapshot, &mut spill, is_interrupted);
        if let Err(error) = result {
            let _ = spill.abort(&spill_connection);
            return Err(error);
        }
        let total = match spill
            .finish(&spill_connection, is_interrupted)
            .and_then(|total| {
                spill.cleanup_aux(&spill_connection)?;
                Ok(total)
            }) {
            Ok(total) => total,
            Err(error) => {
                let _ = spill.abort(&spill_connection);
                return Err(error);
            }
        };
        self.barrier = BarrierState::Ready(spill.into_output(total));
        self.spill_connection = Some(spill_connection);
        Ok(())
    }

    fn populate_sort_spill(
        &mut self,
        spill_connection: &Connection,
        snapshot: &Snapshot<'_>,
        spill: &mut SortSpill,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        while let Some((binding, row)) = self.next_spill_row(snapshot, is_interrupted)? {
            let keys = order_values(&self.prepared, snapshot, &binding, &row)?;
            spill.push(spill_connection, row, keys)?;
        }
        Ok(())
    }

    fn next_spill_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Option<(BindingRow, Vec<Value>)>> {
        check_interrupted(is_interrupted)?;
        let Some(binding) = self.pipeline.next_row(
            snapshot,
            &self.prepared.graph_view,
            &self.prepared.params,
            &mut self.metrics,
        )?
        else {
            return Ok(None);
        };
        let row = project_row(&self.prepared, snapshot, &binding)?;
        Ok(Some((binding, row)))
    }

    fn next_spilled(
        &mut self,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        let spill_connection = self
            .spill_connection
            .take()
            .ok_or_else(|| QueryError::internal("spill output is missing its SQLite connection"))?;
        let state = std::mem::replace(&mut self.barrier, BarrierState::Direct);
        let BarrierState::Ready(mut output) = state else {
            return Err(QueryError::internal(
                "spill output requested before barrier completion",
            ));
        };
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            if is_interrupted() {
                let _ = drop_temp_table(&spill_connection, &output.table);
                self.finished = true;
                return Err(QueryError::interrupted());
            }
            if self
                .prepared
                .limit
                .is_some_and(|limit| self.emitted >= limit)
            {
                drop_temp_table(&spill_connection, &output.table)?;
                return self.finish(rows);
            }
            let Some((seq, row)) = read_output_row(&spill_connection, &output.table, output.after)?
            else {
                drop_temp_table(&spill_connection, &output.table)?;
                return self.finish(rows);
            };
            output.after = seq;
            let at_end = usize::try_from(seq + 1).is_ok_and(|position| position >= output.total);
            if self.skipped < self.prepared.skip {
                self.skipped += 1;
                if at_end {
                    drop_temp_table(&spill_connection, &output.table)?;
                    return self.finish(rows);
                }
                continue;
            }
            self.emitted += 1;
            self.metrics.rows = self.metrics.rows.saturating_add(1);
            rows.push(row);
            if at_end {
                drop_temp_table(&spill_connection, &output.table)?;
                return self.finish(rows);
            }
        }
        self.barrier = BarrierState::Ready(output);
        self.spill_connection = Some(spill_connection);
        Ok(QueryBatch {
            rows,
            done: false,
            summary: None,
        })
    }

    fn finish(&mut self, rows: Vec<Vec<Value>>) -> QueryResult<QueryBatch> {
        self.metrics.elapsed_micros =
            self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.finished = true;
        Ok(QueryBatch {
            rows,
            done: true,
            summary: Some(QuerySummary {
                commit: format!("commit/{}", self.prepared.commit.to_hex()),
                metrics: self.metrics.clone(),
            }),
        })
    }

    pub fn cancel(&mut self, _connection: &Connection) -> QueryResult<()> {
        if let BarrierState::Ready(output) = &self.barrier {
            let spill_connection = self.spill_connection.take().ok_or_else(|| {
                QueryError::internal("spill output is missing its SQLite connection")
            })?;
            drop_temp_table(&spill_connection, &output.table)?;
        }
        self.finished = true;
        Ok(())
    }
}

fn check_interrupted(is_interrupted: &dyn Fn() -> bool) -> QueryResult<()> {
    if is_interrupted() {
        Err(QueryError::interrupted())
    } else {
        Ok(())
    }
}

fn project_row(
    prepared: &PreparedQuery,
    snapshot: &Snapshot<'_>,
    row: &BindingRow,
) -> QueryResult<Vec<Value>> {
    prepared
        .projections
        .iter()
        .map(|projection| {
            expression::evaluate(&projection.expression, snapshot, row, &prepared.params)
        })
        .collect()
}

fn order_values(
    prepared: &PreparedQuery,
    snapshot: &Snapshot<'_>,
    binding: &BindingRow,
    projected: &[Value],
) -> QueryResult<Vec<Value>> {
    let aliases = prepared
        .columns
        .iter()
        .cloned()
        .zip(projected.iter().cloned())
        .collect::<std::collections::BTreeMap<_, _>>();
    prepared
        .order
        .iter()
        .map(|item| {
            expression::evaluate_with_aliases(
                &item.expression,
                snapshot,
                binding,
                &prepared.params,
                &aliases,
            )
        })
        .collect()
}
