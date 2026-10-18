use std::time::Instant;

use chrono::{DateTime, Utc};
use rusqlite::Connection;

use crate::cypher::{ExecutionMode, Value};
use crate::storage::{HashId, LabelId, RelationshipRecord, Snapshot};

use super::completeness::PreparedProgram;
use super::expression::{self, BindingRow, BindingValue};
use super::graph::ResolvedGraphView;
use super::plan::{Direction, MatchStep, PatternPart, PreparedQuery, RelationshipSpec};
use super::spill::{
    BindingSpill, DistinctSpill, RowSpill, SortSpill, SpillOutput, drop_temp_table,
    open_spill_connection, read_output_row_with_resources,
};
use super::{ExecutionControl, MemoryReservation, QueryError, QueryResult, check_interrupted};

mod match_pipeline;
mod node_match;
mod profile;
pub(crate) mod program;
mod program_cursor;
mod project;
#[cfg(test)]
mod resources_tests;
mod transaction;
mod write;
pub(crate) use match_pipeline::spill_match_step;
use match_pipeline::{MatchPipeline, StepCursor};
use node_match::{node_matches, node_matches_scanned};
pub use profile::{OperatorRuntimeMetrics, QueryMetrics};
use project::{order_values, project_row};

const PIPELINE_BATCH: usize = 256;

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

#[derive(Debug, PartialEq, Eq)]
pub struct QuerySummary {
    pub query_type: QueryType,
    pub commit: Option<String>,
    pub merge_session: Option<MergeSessionSummary>,
    pub counters: QueryCounters,
    pub metrics: QueryMetrics,
}

impl QuerySummary {
    pub fn try_clone(&self) -> QueryResult<Self> {
        let extra = super::resources::checked_add(
            self.commit
                .as_ref()
                .map_or(0, |value| value.capacity() as u64),
            self.merge_session
                .as_ref()
                .map_or(0, |value| value.id.capacity() as u64),
        )?;
        let metrics = self.metrics.try_clone_with_extra_bytes(extra)?;
        Ok(Self {
            query_type: self.query_type,
            commit: self.commit.clone(),
            merge_session: self.merge_session.clone(),
            counters: self.counters.clone(),
            metrics,
        })
    }
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
) -> QueryResult<QuerySummary> {
    let metrics = metrics.try_clone_with_extra_bytes(72)?;
    Ok(QuerySummary {
        query_type,
        commit: (!suppress_commit).then(|| format!("commit/{}", commit.to_hex())),
        merge_session: None,
        counters,
        metrics,
    })
}

#[derive(Debug)]
pub struct QueryBatch {
    pub rows: Vec<Vec<Value>>,
    pub done: bool,
    pub summary: Option<QuerySummary>,
    // Fields drop in declaration order; release the lease after its payload.
    _memory: Option<MemoryReservation>,
}

impl PartialEq for QueryBatch {
    fn eq(&self, other: &Self) -> bool {
        self.rows == other.rows && self.done == other.done && self.summary == other.summary
    }
}

#[derive(Debug)]
pub struct QueryCursor {
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
    batch_memory: Option<MemoryReservation>,
    barrier: BarrierState,
    spill_connection: Option<Connection>,
    write_state: WriteState,
    program_stream: Option<program::ProgramStream>,
    program_stream_checked: bool,
    read_summary_commit: Option<HashId>,
    transaction_stream: Option<super::transaction::TransactionProgramStream>,
    transaction_stream_checked: bool,
    transaction_rows: Option<super::mutation::WriteRows>,
    transaction_offset: usize,
    transaction_summary: Option<QuerySummary>,
    finished: bool,
    // Retained seek/parameter leases must outlive the pipeline that owns them.
    prepared: PreparedQuery,
}

#[derive(Debug)]
enum WriteState {
    None,
    Pending,
    Active {
        savepoint: String,
        rows: Option<Box<super::mutation::WriteRows>>,
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
    control: &ExecutionControl<'_>,
) -> QueryResult<Snapshot<'connection>> {
    if let Some(state) = prepared.resolved_state.as_ref() {
        return Ok(Snapshot::from_resolved_state(connection, state));
    }
    match prepared.candidate.as_ref() {
        Some(candidate) => super::graph::resolve_snapshot_with_layer_and_schema_controlled(
            connection,
            prepared.commit,
            &candidate.layer,
            candidate.schema.clone(),
            control,
        ),
        None => super::graph::resolve_snapshot_with_control(connection, prepared.commit, control),
    }
}

impl QueryCursor {
    pub fn new(mut prepared: PreparedQuery) -> Self {
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
        let metrics = std::mem::take(&mut prepared.metrics);
        Self {
            pipeline: MatchPipeline::new(std::mem::take(&mut prepared.matches)),
            prepared,
            metrics,
            started: Instant::now(),
            statement_time,
            transaction_time: statement_time,
            suppress_summary_commit: false,
            skipped: 0,
            emitted: 0,
            explain_emitted: false,
            aggregate_state: None,
            batch_memory: None,
            barrier,
            spill_connection: None,
            write_state,
            program_stream: None,
            program_stream_checked: false,
            read_summary_commit: None,
            transaction_stream: None,
            transaction_stream_checked: false,
            transaction_rows: None,
            transaction_offset: 0,
            transaction_summary: None,
            finished: false,
        }
    }

    pub fn resources(&self) -> &super::QueryResources {
        &self.prepared.resources
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

    pub fn has_connection_state_operation(&self) -> bool {
        self.prepared
            .program
            .as_ref()
            .is_some_and(|program| program.checkout_operation)
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
        match self.advance_with_interrupt(connection, max_rows, is_interrupted) {
            Ok(mut batch) => {
                batch._memory = self.batch_memory.take();
                Ok(batch)
            }
            Err(error) => {
                let cleanup = self.cancel(connection);
                self.release_working_memory();
                cleanup?;
                Err(error)
            }
        }
    }

    fn release_working_memory(&mut self) {
        self.pipeline.clear();
        self.prepared.params.clear();
        self.prepared.params_memory = None;
        self.prepared.resolved_state = None;
        if let Some(candidate) = &mut self.prepared.candidate {
            candidate.release_working_memory();
        }
        self.program_stream = None;
        self.transaction_stream = None;
        self.transaction_rows = None;
        self.aggregate_state = None;
        self.metrics = QueryMetrics::default();
        self.batch_memory = None;
    }

    fn advance_with_interrupt(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        self.batch_memory = None;
        let _statement_clock = super::functions::install_statement_time(self.statement_time);
        let _transaction_clock = super::functions::install_transaction_time(self.transaction_time);
        if self.finished {
            return Ok(QueryBatch {
                _memory: None,
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
        let resources = self.prepared.resources.clone();
        let control = ExecutionControl::new(&resources, is_interrupted);
        let snapshot = prepared_snapshot(connection, &self.prepared, &control)?;
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
        let text = self.prepared.physical.explain();
        self.batch_memory = Some(self.prepared.resources.reserve(
            super::resources::checked_add(
                text.capacity() as u64,
                std::mem::size_of::<Value>() as u64,
            )?,
        )?);
        self.metrics.rows = 1;
        self.finish(vec![vec![Value::String(text)]])
    }

    fn next_direct(
        &mut self,
        snapshot: &Snapshot<'_>,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        let resources = self.prepared.resources.clone();
        let control = ExecutionControl::new(&resources, is_interrupted);
        let mut memory =
            control.reserve(super::resources::allocation_bytes::<Vec<Value>>(max_rows)?)?;
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            check_interrupted(is_interrupted)?;
            if self
                .prepared
                .limit
                .is_some_and(|limit| self.emitted >= limit)
            {
                self.batch_memory = Some(memory);
                return self.finish(rows);
            }
            let next = self.pipeline.next_row(
                snapshot,
                &self.prepared.graph_view,
                &self.prepared.params,
                &mut self.metrics,
                &control,
            )?;
            let Some(binding) = next else {
                self.batch_memory = Some(memory);
                return self.finish(rows);
            };
            self.metrics.record_active_rows(1);
            if self.skipped < self.prepared.skip {
                self.skipped += 1;
                continue;
            }
            let projected = project_row(&self.prepared, snapshot, &binding, &control)?;
            memory.merge(projected.memory)?;
            rows.push(projected.values);
            self.emitted += 1;
            self.metrics.rows = self.metrics.rows.saturating_add(1);
        }
        self.batch_memory = Some(memory);
        Ok(QueryBatch {
            _memory: None,
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
        let mut spill = DistinctSpill::create_with_resources(
            &spill_connection,
            self.prepared.resources.clone(),
        )?;
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
        let resources = self.prepared.resources.clone();
        let control = ExecutionControl::new(&resources, is_interrupted);
        while let Some((_, row)) = self.next_spill_row(snapshot, is_interrupted)? {
            spill.push(spill_connection, &row.values, &control)?;
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
        let mut spill = SortSpill::create_with_resources(
            &spill_connection,
            self.prepared.distinct,
            directions,
            self.prepared.resources.clone(),
        )?;
        let result =
            self.populate_sort_spill(&spill_connection, snapshot, &mut spill, is_interrupted);
        if let Err(error) = result {
            let _ = spill.abort(&spill_connection);
            return Err(error);
        }
        let resources = self.prepared.resources.clone();
        let control = ExecutionControl::new(&resources, is_interrupted);
        let total = match spill.finish(&spill_connection, &control).and_then(|total| {
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
            let resources = self.prepared.resources.clone();
            let control = ExecutionControl::new(&resources, is_interrupted);
            let keys = order_values(&self.prepared, snapshot, &binding, &row.values, &control)?;
            spill.push_reserved(
                spill_connection,
                row.values,
                keys.values,
                row.memory,
                keys.memory,
                &control,
            )?;
        }
        Ok(())
    }

    fn next_spill_row(
        &mut self,
        snapshot: &Snapshot<'_>,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Option<(BindingRow, project::ProjectedRow)>> {
        let resources = self.prepared.resources.clone();
        let control = ExecutionControl::new(&resources, is_interrupted);
        control.check()?;
        let Some(binding) = self.pipeline.next_row(
            snapshot,
            &self.prepared.graph_view,
            &self.prepared.params,
            &mut self.metrics,
            &control,
        )?
        else {
            return Ok(None);
        };
        self.metrics.record_active_rows(1);
        let row = project_row(&self.prepared, snapshot, &binding, &control)?;
        Ok(Some((binding, row)))
    }

    fn limit_reached(&self) -> bool {
        self.prepared
            .limit
            .is_some_and(|limit| self.emitted >= limit)
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
        let mut memory = self
            .prepared
            .resources
            .reserve(super::resources::allocation_bytes::<Vec<Value>>(max_rows)?)?;
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            if is_interrupted() {
                let _ = drop_temp_table(&spill_connection, &output.table);
                self.finished = true;
                return Err(QueryError::interrupted());
            }
            if self.limit_reached() {
                drop_temp_table(&spill_connection, &output.table)?;
                self.batch_memory = Some(memory);
                return self.finish(rows);
            }
            let Some((row, row_memory, at_end)) =
                self.next_visible_spill_row(&spill_connection, &mut output, is_interrupted)?
            else {
                drop_temp_table(&spill_connection, &output.table)?;
                self.batch_memory = Some(memory);
                return self.finish(rows);
            };
            self.emitted += 1;
            self.metrics.rows = self.metrics.rows.saturating_add(1);
            memory.merge(row_memory)?;
            rows.push(row);
            if at_end {
                drop_temp_table(&spill_connection, &output.table)?;
                self.batch_memory = Some(memory);
                return self.finish(rows);
            }
        }
        self.barrier = BarrierState::Ready(output);
        self.spill_connection = Some(spill_connection);
        self.batch_memory = Some(memory);
        Ok(QueryBatch {
            _memory: None,
            rows,
            done: false,
            summary: None,
        })
    }

    fn next_visible_spill_row(
        &mut self,
        connection: &Connection,
        output: &mut SpillOutput,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Option<(Vec<Value>, MemoryReservation, bool)>> {
        let resources = self.prepared.resources.clone();
        let control = ExecutionControl::new(&resources, is_interrupted);
        loop {
            check_interrupted(is_interrupted)?;
            let Some((sequence, row, memory)) =
                read_output_row_with_resources(connection, &output.table, output.after, &control)?
            else {
                return Ok(None);
            };
            output.after = sequence;
            let at_end = spill_at_end(sequence, output.total);
            if self.skipped < self.prepared.skip {
                self.skipped += 1;
                if at_end {
                    return Ok(None);
                }
                continue;
            }
            return Ok(Some((row, memory, at_end)));
        }
    }

    fn finish(&mut self, rows: Vec<Vec<Value>>) -> QueryResult<QueryBatch> {
        self.pipeline.clear();
        self.prepared.params.clear();
        self.prepared.params_memory = None;
        self.prepared.resolved_state = None;
        if let Some(candidate) = &mut self.prepared.candidate {
            candidate.release_working_memory();
        }
        self.program_stream = None;
        self.metrics.elapsed_micros =
            self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.metrics.finish_profile();
        self.finished = true;
        Ok(QueryBatch {
            _memory: None,
            rows,
            done: true,
            summary: Some(self.read_summary()?),
        })
    }

    fn read_summary(&self) -> QueryResult<QuerySummary> {
        let extra = super::resources::checked_add(
            72,
            self.prepared
                .candidate
                .as_ref()
                .map_or(0, |candidate| candidate.session_id.capacity() as u64),
        )?;
        let metrics = self.metrics.try_clone_with_extra_bytes(extra)?;
        Ok(QuerySummary {
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
                (!self.suppress_summary_commit).then(|| {
                    format!(
                        "commit/{}",
                        self.read_summary_commit
                            .unwrap_or(self.prepared.commit)
                            .to_hex()
                    )
                })
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
            metrics,
        })
    }
}

fn spill_at_end(sequence: i64, total: usize) -> bool {
    usize::try_from(sequence.saturating_add(1)).is_ok_and(|position| position >= total)
}
