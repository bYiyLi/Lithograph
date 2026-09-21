use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

static NEXT_WRITE_SAVEPOINT: AtomicU64 = AtomicU64::new(1);

struct CursorWriteOutcome {
    rows: Option<super::super::mutation::WriteRows>,
    commit: crate::storage::HashId,
    counters: QueryCounters,
    query_type: QueryType,
    suppress_commit: bool,
}

impl QueryCursor {
    pub(super) fn next_write(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        if self
            .prepared
            .program
            .as_ref()
            .is_some_and(|program| program.checkout_operation)
            && !connection.is_autocommit()
        {
            self.finished = true;
            return Err(QueryError::transaction_boundary_required(
                "Branch checkout requires SQLite autocommit mode",
            ));
        }
        self.start_pending_write(connection, is_interrupted)?;
        self.cancel_interrupted_write(connection, is_interrupted)?;
        match self.next_active_write_batch(connection, max_rows, is_interrupted) {
            Ok(batch) => Ok(batch),
            Err(error) => {
                let savepoint = match &self.write_state {
                    WriteState::Active { savepoint, .. } => Some(savepoint.clone()),
                    _ => None,
                };
                self.finished = true;
                self.write_state = WriteState::None;
                if let Some(savepoint) = savepoint
                    && let Err(cleanup) = rollback_write_savepoint(connection, &savepoint)
                {
                    return Err(cleanup);
                }
                Err(error)
            }
        }
    }

    fn start_pending_write(
        &mut self,
        connection: &Connection,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if !matches!(self.write_state, WriteState::Pending) {
            return Ok(());
        }
        let retry_version_busy = connection.is_autocommit()
            && self
                .prepared
                .program
                .as_ref()
                .is_some_and(super::super::completeness::retry_version_busy);
        let original_metrics = self.metrics.clone();
        for attempt in 0..2 {
            if attempt != 0 {
                self.metrics = original_metrics.clone();
            }
            let ordinal = NEXT_WRITE_SAVEPOINT.fetch_add(1, Ordering::Relaxed);
            let savepoint = format!("lithograph_write_{ordinal}");
            connection.execute_batch(&format!("SAVEPOINT {savepoint}"))?;
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                self.execute_pending_write(connection, is_interrupted)
            }));
            match outcome {
                Ok(Ok(outcome)) => {
                    self.install_active_write(savepoint, outcome);
                    return Ok(());
                }
                Ok(Err(error)) => {
                    if let Err(cleanup) = rollback_write_savepoint(connection, &savepoint) {
                        self.finished = true;
                        self.write_state = WriteState::None;
                        return Err(cleanup);
                    }
                    let retry = attempt == 0
                        && retry_version_busy
                        && error.sqlite_code == Some(rusqlite::ffi::SQLITE_BUSY)
                        && !is_interrupted();
                    if retry {
                        continue;
                    }
                    self.finished = true;
                    return Err(error);
                }
                Err(_) => {
                    self.finished = true;
                    rollback_write_savepoint(connection, &savepoint)?;
                    return Err(QueryError::internal(
                        "panic while executing a Lithograph mutating query",
                    ));
                }
            }
        }
        Err(QueryError::internal(
            "Version Procedure optimistic write retry did not terminate",
        ))
    }

    fn execute_pending_write(
        &mut self,
        connection: &Connection,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<CursorWriteOutcome> {
        if let Some(write) = self.prepared.write.as_ref() {
            return super::super::mutation::execute_write(
                connection,
                write,
                self.prepared.commit,
                &self.prepared.params,
                &mut self.metrics,
                is_interrupted,
            )
            .map(write_cursor_outcome);
        }
        if let Some(schema) = self.prepared.schema.as_ref() {
            return super::super::schema::execute_schema(
                connection,
                schema,
                self.prepared.commit,
                is_interrupted,
            )
            .map(|outcome| CursorWriteOutcome {
                rows: None,
                commit: outcome.commit,
                counters: schema_query_counters(outcome.counters),
                query_type: QueryType::Schema,
                suppress_commit: false,
            });
        }
        let Some(program) = self.prepared.program.as_ref() else {
            return Err(QueryError::internal(
                "write query is missing its mutation plan",
            ));
        };
        if program.version_mutation && !program.writes {
            let (rows, commit) = super::super::completeness::execute_version_program(
                connection,
                program,
                self.prepared.commit,
                &self.prepared.graph_view,
                &self.prepared.params,
                &mut self.metrics,
                is_interrupted,
            )?;
            return Ok(CursorWriteOutcome {
                rows: super::super::mutation::spill_value_rows(rows)?,
                commit,
                counters: QueryCounters::default(),
                query_type: QueryType::Version,
                suppress_commit: super::super::completeness::suppress_version_summary_commit(
                    program,
                ),
            });
        }
        super::super::mutation::execute_program(
            connection,
            program,
            self.prepared.commit,
            &self.prepared.params,
            &mut self.metrics,
            is_interrupted,
        )
        .map(write_cursor_outcome)
    }

    fn install_active_write(&mut self, savepoint: String, mut outcome: CursorWriteOutcome) {
        if self.prepared.columns.is_empty() {
            outcome.rows = None;
        }
        self.metrics.rows = outcome
            .rows
            .as_ref()
            .map_or(0, |rows| rows.total().try_into().unwrap_or(u64::MAX));
        let summary = super::committed_summary(
            outcome.query_type,
            outcome.commit,
            outcome.counters,
            &self.metrics,
            self.suppress_summary_commit || outcome.suppress_commit,
        );
        self.write_state = WriteState::Active {
            savepoint,
            rows: outcome.rows.map(Box::new),
            offset: 0,
            summary,
        };
    }

    fn cancel_interrupted_write(
        &mut self,
        connection: &Connection,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if !is_interrupted() {
            return Ok(());
        }
        let savepoint = match &self.write_state {
            WriteState::Active { savepoint, .. } => savepoint.clone(),
            _ => {
                return Err(QueryError::internal(
                    "write cursor reached an invalid execution state",
                ));
            }
        };
        self.finished = true;
        rollback_write_savepoint(connection, &savepoint)?;
        self.write_state = WriteState::None;
        Err(QueryError::interrupted())
    }

    fn next_active_write_batch(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        let WriteState::Active {
            savepoint: _,
            rows,
            offset,
            summary,
        } = &mut self.write_state
        else {
            return Err(QueryError::internal(
                "write cursor reached an invalid execution state",
            ));
        };
        let Some(rows) = rows.as_mut() else {
            self.metrics.elapsed_micros =
                self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            self.metrics.finish_profile();
            summary.metrics = self.metrics.clone();
            return Ok(QueryBatch {
                rows: Vec::new(),
                done: true,
                summary: Some(summary.clone()),
            });
        };
        let batch_rows = rows.next_batch(connection, max_rows, is_interrupted)?;
        *offset = offset.saturating_add(batch_rows.len());
        let done = *offset >= rows.total();
        if done {
            self.metrics.elapsed_micros =
                self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            self.metrics.finish_profile();
            summary.metrics = self.metrics.clone();
        }
        Ok(QueryBatch {
            rows: batch_rows,
            done,
            summary: done.then(|| summary.clone()),
        })
    }

    pub fn complete(&mut self, connection: &Connection) -> QueryResult<QuerySummary> {
        if let Some(summary) = self.transaction_summary.clone() {
            if self.transaction_stream.is_some() {
                if !self.finished {
                    return Err(QueryError::internal(
                        "transaction stream cannot complete before its terminal batch",
                    ));
                }
            } else {
                let total = self
                    .transaction_rows
                    .as_ref()
                    .map_or(0, super::super::mutation::WriteRows::total);
                if self.transaction_offset < total {
                    return Err(QueryError::internal(
                        "transaction query cannot complete before execution reaches a terminal batch",
                    ));
                }
            }
            self.finished = true;
            return Ok(summary);
        }
        match std::mem::replace(&mut self.write_state, WriteState::None) {
            WriteState::Active {
                savepoint,
                rows,
                offset,
                summary,
            } => {
                let total = rows.as_ref().map_or(0, |rows| rows.total());
                if offset < total {
                    self.write_state = WriteState::Active {
                        savepoint,
                        rows,
                        offset,
                        summary,
                    };
                    return Err(QueryError::internal(
                        "write cursor cannot complete before execution reaches a terminal batch",
                    ));
                }
                if let Err(error) = connection.execute_batch(&format!("RELEASE {savepoint}")) {
                    let release_error = QueryError::from(error);
                    self.finished = true;
                    rollback_write_savepoint(connection, &savepoint)?;
                    return Err(release_error);
                }
                self.finished = true;
                self.write_state = WriteState::Completed(summary.clone());
                Ok(summary)
            }
            WriteState::Completed(summary) => {
                self.write_state = WriteState::Completed(summary.clone());
                Ok(summary)
            }
            WriteState::Pending => {
                self.write_state = WriteState::Pending;
                Err(QueryError::internal(
                    "write cursor cannot complete before execution reaches a terminal batch",
                ))
            }
            WriteState::None => {
                self.write_state = WriteState::None;
                if !self.finished {
                    return Err(QueryError::internal(
                        "read cursor cannot complete before execution reaches a terminal batch",
                    ));
                }
                Ok(self.read_summary())
            }
        }
    }

    pub fn complete_with_interrupt(
        &mut self,
        connection: &Connection,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QuerySummary> {
        if is_interrupted() {
            self.cancel(connection)?;
            return Err(QueryError::interrupted());
        }
        self.complete(connection)
    }

    pub fn cancel(&mut self, connection: &Connection) -> QueryResult<()> {
        if let Some(stream) = self.transaction_stream.as_mut() {
            stream.cancel()?;
            self.finished = true;
        }
        if self.transaction_summary.is_some() {
            self.finished = true;
            return Ok(());
        }
        if let WriteState::Active { savepoint, .. } = &self.write_state {
            let savepoint = savepoint.clone();
            rollback_write_savepoint(connection, &savepoint)?;
        }
        if let BarrierState::Ready(output) = &self.barrier {
            let spill_connection = self.spill_connection.take().ok_or_else(|| {
                QueryError::internal("spill output is missing its SQLite connection")
            })?;
            drop_temp_table(&spill_connection, &output.table)?;
        }
        self.write_state = WriteState::None;
        self.finished = true;
        Ok(())
    }
}

fn query_counters(counters: super::super::mutation::MutationCounters) -> QueryCounters {
    QueryCounters {
        nodes_created: counters.nodes_created,
        nodes_deleted: counters.nodes_deleted,
        relationships_created: counters.relationships_created,
        relationships_deleted: counters.relationships_deleted,
        properties_set: counters.properties_set,
        properties_removed: counters.properties_removed,
        labels_added: counters.labels_added,
        labels_removed: counters.labels_removed,
        ..QueryCounters::default()
    }
}

fn write_cursor_outcome(outcome: super::super::mutation::WriteOutcome) -> CursorWriteOutcome {
    CursorWriteOutcome {
        rows: outcome.rows,
        commit: outcome.commit,
        counters: query_counters(outcome.counters),
        query_type: QueryType::Write,
        suppress_commit: false,
    }
}

fn schema_query_counters(counters: super::super::schema::SchemaCounters) -> QueryCounters {
    QueryCounters {
        constraints_added: counters.constraints_added,
        constraints_removed: counters.constraints_removed,
        indexes_added: counters.indexes_added,
        indexes_removed: counters.indexes_removed,
        ..QueryCounters::default()
    }
}

fn rollback_write_savepoint(connection: &Connection, savepoint: &str) -> QueryResult<()> {
    if let Err(error) = connection.execute_batch(&format!("ROLLBACK TO {savepoint}")) {
        return fail_write_savepoint_cleanup(
            connection,
            format!("rollback-to-savepoint failed ({error})"),
        );
    }
    if let Err(error) = connection.execute_batch(&format!("RELEASE {savepoint}")) {
        return fail_write_savepoint_cleanup(
            connection,
            format!("release-savepoint failed ({error})"),
        );
    }
    Ok(())
}

fn fail_write_savepoint_cleanup(connection: &Connection, detail: String) -> QueryResult<()> {
    let suffix = if connection.execute_batch("ROLLBACK").is_ok() {
        "full SQLite rollback executed"
    } else {
        "full SQLite rollback also failed"
    };
    Err(QueryError::internal(format!(
        "write savepoint cleanup failed ({detail}); {suffix}"
    )))
}
