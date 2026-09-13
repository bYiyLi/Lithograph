use std::time::Instant;

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::cypher::{ExecutionMode, Value};
use crate::storage::{HashId, RelationshipRecord, Snapshot};

use super::completeness::PreparedProgram;
use super::expression::{self, BindingRow, BindingValue};
use super::graph::ResolvedGraphView;
use super::plan::{Direction, MatchStep, NodeSpec, PatternPart, PreparedQuery, RelationshipSpec};
use super::spill::{
    DistinctSpill, SortSpill, SpillOutput, drop_temp_table, open_spill_connection, read_output_row,
};
use super::{QueryError, QueryResult};

mod project;
mod write;
use project::{order_values, project_row};

const PIPELINE_BATCH: usize = 256;

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryMetrics {
    pub rows: u64,
    pub db_hits: u64,
    pub elapsed_micros: u64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueryType {
    Read,
    Write,
    Schema,
    Version,
    Mixed,
}

impl QueryType {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Read => "read",
            Self::Write => "write",
            Self::Schema => "schema",
            Self::Version => "version",
            Self::Mixed => "mixed",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct QueryCounters {
    pub nodes_created: u64,
    pub nodes_deleted: u64,
    pub relationships_created: u64,
    pub relationships_deleted: u64,
    pub properties_set: u64,
    pub properties_removed: u64,
    pub labels_added: u64,
    pub labels_removed: u64,
    pub constraints_added: u64,
    pub constraints_removed: u64,
    pub indexes_added: u64,
    pub indexes_removed: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuerySummary {
    pub query_type: QueryType,
    pub commit: Option<String>,
    pub merge_session: Option<MergeSessionSummary>,
    pub counters: QueryCounters,
    pub metrics: QueryMetrics,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MergeSessionSummary {
    pub id: String,
    pub revision: i64,
}

fn committed_summary(
    query_type: QueryType,
    commit: HashId,
    counters: QueryCounters,
    metrics: &QueryMetrics,
    suppress_commit: bool,
) -> QuerySummary {
    QuerySummary {
        query_type,
        commit: (!suppress_commit).then(|| format!("commit/{}", commit.to_hex())),
        merge_session: None,
        counters,
        metrics: metrics.clone(),
    }
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
    statement_time: DateTime<Utc>,
    transaction_time: DateTime<Utc>,
    suppress_summary_commit: bool,
    skipped: usize,
    emitted: usize,
    explain_emitted: bool,
    aggregate_state: Option<Vec<u64>>,
    barrier: BarrierState,
    spill_connection: Option<Connection>,
    write_state: WriteState,
    program_rows: Option<Vec<Vec<Value>>>,
    program_offset: usize,
    transaction_summary: Option<QuerySummary>,
    finished: bool,
}

#[derive(Debug)]
enum WriteState {
    None,
    Pending,
    Active {
        savepoint: String,
        rows: Vec<Vec<Value>>,
        offset: usize,
        summary: QuerySummary,
    },
    Completed(QuerySummary),
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
    if let Some(variable) = variable
        && !row.values.contains_key(variable)
    {
        row.insert(variable.to_owned(), BindingValue::Null);
    }
}

pub(crate) fn materialize_match_step(
    snapshot: &Snapshot<'_>,
    graph_view: &ResolvedGraphView,
    params: &std::collections::BTreeMap<String, Value>,
    input: Vec<BindingRow>,
    step: &MatchStep,
    metrics: &mut QueryMetrics,
    is_interrupted: &dyn Fn() -> bool,
) -> QueryResult<Vec<BindingRow>> {
    let mut output = Vec::new();
    for row in input {
        let mut cursor = StepCursor::new(step.clone(), row);
        loop {
            check_interrupted(is_interrupted)?;
            let Some(row) = cursor.next_row(snapshot, graph_view, params, metrics)? else {
                break;
            };
            output.push(row);
        }
    }
    Ok(output)
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
    indexed_relationship_after: i64,
    indexed_relationship_ids: Vec<i64>,
    indexed_relationship_index: usize,
    indexed_relationship_done: bool,
    indexed_rows: Vec<BindingRow>,
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
            indexed_relationship_after: 0,
            indexed_relationship_ids: Vec::new(),
            indexed_relationship_index: 0,
            indexed_relationship_done: false,
            indexed_rows: Vec::new(),
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
        if self
            .part
            .relationship
            .as_ref()
            .is_some_and(|relationship| relationship.index_seek.is_some())
        {
            return self.next_indexed_relationship_row(snapshot, graph_view, metrics);
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
        self.next_relationship_row(snapshot, graph_view, metrics)
    }

    fn next_relationship_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<BindingRow>> {
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

    fn next_indexed_relationship_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<BindingRow>> {
        loop {
            if let Some(row) = self.indexed_rows.pop() {
                return Ok(Some(row));
            }
            while self.indexed_relationship_index < self.indexed_relationship_ids.len() {
                let id = self.indexed_relationship_ids[self.indexed_relationship_index];
                self.indexed_relationship_index += 1;
                let Some(relationship) = snapshot.relationship(id)? else {
                    continue;
                };
                let Some(spec) = self.part.relationship.as_ref() else {
                    return Ok(None);
                };
                let orientations = match spec.direction {
                    Direction::Outgoing => vec![(relationship.source, relationship.target)],
                    Direction::Incoming => vec![(relationship.target, relationship.source)],
                    Direction::Undirected if relationship.source == relationship.target => {
                        vec![(relationship.source, relationship.target)]
                    }
                    Direction::Undirected => vec![
                        (relationship.target, relationship.source),
                        (relationship.source, relationship.target),
                    ],
                };
                for (start, end) in orientations {
                    if let Some(row) = self.match_indexed_relationship_orientation(
                        snapshot,
                        graph_view,
                        relationship,
                        start,
                        end,
                        metrics,
                    )? {
                        self.indexed_rows.push(row);
                    }
                }
                if let Some(row) = self.indexed_rows.pop() {
                    return Ok(Some(row));
                }
            }
            if self.indexed_relationship_done {
                return Ok(None);
            }
            let Some(spec) = self.part.relationship.as_ref() else {
                return Ok(None);
            };
            let Some(seek) = spec.index_seek.as_ref() else {
                return Ok(None);
            };
            let page = super::schema::scan_relationship_index_after(
                snapshot,
                seek,
                spec.type_id,
                self.indexed_relationship_after,
                PIPELINE_BATCH,
            )?;
            metrics.db_hits = metrics.db_hits.saturating_add(page.items.len() as u64);
            self.indexed_relationship_ids = page.items;
            self.indexed_relationship_index = 0;
            if let Some(after) = page.next_after {
                self.indexed_relationship_after = after;
            } else {
                self.indexed_relationship_done = true;
            }
        }
    }

    fn match_indexed_relationship_orientation(
        &self,
        snapshot: &Snapshot<'_>,
        graph_view: &ResolvedGraphView,
        relationship: RelationshipRecord,
        start: i64,
        end: i64,
        metrics: &mut QueryMetrics,
    ) -> QueryResult<Option<BindingRow>> {
        let Some(spec) = &self.part.relationship else {
            return Ok(None);
        };
        let Some(end_spec) = &self.part.end else {
            return Ok(None);
        };
        if !graph_view.visible_relationship(snapshot, relationship)? {
            return Ok(None);
        }
        if !node_matches(snapshot, graph_view, &self.part.start, start, metrics)?
            || !node_matches(snapshot, graph_view, end_spec, end, metrics)?
        {
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
        let Some(row) = bind_node(row, end_spec.variable.as_deref(), end)? else {
            return Ok(None);
        };
        let Some(mut row) = bind_path(
            row,
            self.part.path_variable.as_deref(),
            vec![start, end],
            vec![relationship],
        )?
        else {
            return Ok(None);
        };
        row.used_relationships.insert(relationship.id);
        Ok(Some(row))
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
            let page = if let Some(seek) = &self.part.start.index_seek
                && seek.kind != crate::storage::StandardIndexKind::Lookup
            {
                super::schema::scan_node_index_after(
                    snapshot,
                    seek,
                    self.start_after,
                    PIPELINE_BATCH,
                )?
            } else {
                let scan_label = self
                    .part
                    .start
                    .scan_label
                    .or_else(|| graph_view.scan_label());
                if let Some(label) = scan_label {
                    snapshot.scan_label_after(label, self.start_after, PIPELINE_BATCH)?
                } else {
                    snapshot.scan_nodes_after(self.start_after, PIPELINE_BATCH)?
                }
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
            row.insert(variable.to_owned(), BindingValue::Node(node_id));
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
            row.insert(
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
            row.insert(
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

fn prepared_program<'a>(
    prepared: &'a PreparedQuery,
    missing_message: &str,
) -> QueryResult<&'a PreparedProgram> {
    prepared
        .program
        .as_ref()
        .ok_or_else(|| QueryError::internal(missing_message))
}

fn prepared_snapshot<'connection>(
    connection: &'connection Connection,
    prepared: &PreparedQuery,
) -> QueryResult<Snapshot<'connection>> {
    match prepared.candidate.as_ref() {
        Some(candidate) => Ok(Snapshot::resolve_with_layer_and_schema(
            connection,
            prepared.commit,
            &candidate.layer,
            candidate.schema.clone(),
        )?),
        None => Ok(Snapshot::resolve(connection, prepared.commit)?),
    }
}

impl QueryCursor {
    pub fn new(prepared: PreparedQuery) -> Self {
        let transaction_boundary = prepared.requires_transaction_boundary();
        let barrier = if prepared.order.is_empty() && !prepared.distinct {
            BarrierState::Direct
        } else {
            BarrierState::Pending
        };
        let write_state = if !transaction_boundary
            && (prepared.write.is_some()
                || prepared.schema.is_some()
                || prepared
                    .program
                    .as_ref()
                    .is_some_and(|program| program.writes || program.version_mutation))
            && prepared.mode != ExecutionMode::Explain
        {
            WriteState::Pending
        } else {
            WriteState::None
        };
        let statement_time = Utc::now();
        Self {
            pipeline: MatchPipeline::new(prepared.matches.clone()),
            prepared,
            metrics: QueryMetrics::default(),
            started: Instant::now(),
            statement_time,
            transaction_time: statement_time,
            suppress_summary_commit: false,
            skipped: 0,
            emitted: 0,
            explain_emitted: false,
            aggregate_state: None,
            barrier,
            spill_connection: None,
            write_state,
            program_rows: None,
            program_offset: 0,
            transaction_summary: None,
            finished: false,
        }
    }

    pub fn columns(&self) -> &[String] {
        &self.prepared.columns
    }

    pub fn is_write(&self) -> bool {
        (self.prepared.write.is_some()
            || self.prepared.schema.is_some()
            || self
                .prepared
                .program
                .as_ref()
                .is_some_and(|program| program.writes || program.version_mutation))
            && self.prepared.mode != ExecutionMode::Explain
    }

    pub fn requires_transaction_boundary(&self) -> bool {
        self.prepared.requires_transaction_boundary()
    }

    pub fn has_external_io(&self) -> bool {
        self.prepared.has_external_io()
    }

    pub fn has_version_operation(&self) -> bool {
        self.prepared
            .program
            .as_ref()
            .is_some_and(|program| program.version_operation)
    }

    pub fn set_transaction_time_micros(&mut self, micros: i64) -> QueryResult<()> {
        let seconds = micros.div_euclid(1_000_000);
        let micros = micros.rem_euclid(1_000_000) as u32;
        self.transaction_time = DateTime::<Utc>::from_timestamp(seconds, micros * 1_000)
            .ok_or_else(|| QueryError::invalid_argument("transaction timestamp is out of range"))?;
        Ok(())
    }

    pub fn suppress_summary_commit(&mut self) {
        self.suppress_summary_commit = true;
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
        let _statement_clock = super::functions::install_statement_time(self.statement_time);
        let _transaction_clock = super::functions::install_transaction_time(self.transaction_time);
        if self.finished {
            return Ok(QueryBatch {
                rows: Vec::new(),
                done: true,
                summary: None,
            });
        }
        if is_interrupted() {
            self.cancel(connection)?;
            return Err(QueryError::interrupted());
        }
        let max_rows = max_rows.clamp(1, 4_096);
        if self.prepared.mode == ExecutionMode::Explain {
            return self.next_explain();
        }
        if self.prepared.requires_transaction_boundary() {
            return self.next_transaction_program(connection, max_rows, is_interrupted);
        }
        if self.is_write() {
            return self.next_write(connection, max_rows, is_interrupted);
        }
        if self.prepared.program.is_some() {
            return self.next_program_read(connection, max_rows, is_interrupted);
        }
        let snapshot = prepared_snapshot(connection, &self.prepared)?;
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

    fn next_program_read(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        if self.program_rows.is_none() {
            let snapshot = prepared_snapshot(connection, &self.prepared)?;
            let program = prepared_program(&self.prepared, "Phase 06 program is missing")?;
            let rows = super::completeness::execute_prepared_read(
                connection,
                program,
                snapshot,
                &self.prepared.graph_view,
                &self.prepared.params,
                self.prepared.mode,
                &mut self.metrics,
                is_interrupted,
            )?;
            self.metrics.rows = rows.len().try_into().unwrap_or(u64::MAX);
            self.program_rows = Some(rows);
        }
        let (batch_rows, done) =
            self.take_program_rows(max_rows, "Phase 06 program rows are missing")?;
        if done {
            return self.finish(batch_rows);
        }
        Ok(QueryBatch {
            rows: batch_rows,
            done: false,
            summary: None,
        })
    }

    fn next_transaction_program(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        if self.program_rows.is_none() {
            let program = prepared_program(&self.prepared, "transaction program is missing")?;
            let outcome = super::transaction::execute_transaction_program(
                connection,
                program,
                self.prepared.commit,
                &self.prepared.params,
                &mut self.metrics,
                is_interrupted,
            )?;
            self.metrics.rows = outcome.rows.len().try_into().unwrap_or(u64::MAX);
            self.transaction_summary = Some(committed_summary(
                if program.writes {
                    QueryType::Write
                } else {
                    QueryType::Read
                },
                outcome.commit,
                outcome.counters,
                &self.metrics,
                self.suppress_summary_commit,
            ));
            self.program_rows = Some(outcome.rows);
        }
        let (batch_rows, done) =
            self.take_program_rows(max_rows, "transaction program rows are missing")?;
        if done {
            self.metrics.elapsed_micros =
                self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            if let Some(summary) = &mut self.transaction_summary {
                summary.metrics = self.metrics.clone();
            }
        }
        Ok(QueryBatch {
            rows: batch_rows,
            done,
            summary: done.then(|| self.transaction_summary.clone()).flatten(),
        })
    }

    fn take_program_rows(
        &mut self,
        max_rows: usize,
        missing_message: &str,
    ) -> QueryResult<(Vec<Vec<Value>>, bool)> {
        let rows = self
            .program_rows
            .as_ref()
            .ok_or_else(|| QueryError::internal(missing_message))?;
        let end = self.program_offset.saturating_add(max_rows).min(rows.len());
        let batch_rows = rows[self.program_offset..end].to_vec();
        self.program_offset = end;
        Ok((batch_rows, self.program_offset >= rows.len()))
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
            summary: Some(self.read_summary()),
        })
    }

    fn read_summary(&self) -> QuerySummary {
        QuerySummary {
            query_type: if self
                .prepared
                .program
                .as_ref()
                .is_some_and(|program| program.version_operation)
            {
                QueryType::Version
            } else {
                QueryType::Read
            },
            commit: if self.prepared.candidate.is_some() {
                None
            } else {
                (!self.suppress_summary_commit)
                    .then(|| format!("commit/{}", self.prepared.commit.to_hex()))
            },
            merge_session: self
                .prepared
                .candidate
                .as_ref()
                .map(|candidate| MergeSessionSummary {
                    id: candidate.session_id.clone(),
                    revision: candidate.revision,
                }),
            counters: QueryCounters::default(),
            metrics: self.metrics.clone(),
        }
    }
}

fn check_interrupted(is_interrupted: &dyn Fn() -> bool) -> QueryResult<()> {
    if is_interrupted() {
        Err(QueryError::interrupted())
    } else {
        Ok(())
    }
}
