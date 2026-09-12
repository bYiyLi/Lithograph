use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicU64, Ordering};

use super::*;

static NEXT_WRITE_SAVEPOINT: AtomicU64 = AtomicU64::new(1);

impl QueryCursor {
    pub(super) fn next_write(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        self.start_pending_write(connection, is_interrupted)?;
        self.cancel_interrupted_write(connection, is_interrupted)?;
        self.next_active_write_batch(max_rows)
    }

    fn start_pending_write(
        &mut self,
        connection: &Connection,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if matches!(self.write_state, WriteState::Pending) {
            let ordinal = NEXT_WRITE_SAVEPOINT.fetch_add(1, Ordering::Relaxed);
            let savepoint = format!("lithograph_write_{ordinal}");
            connection.execute_batch(&format!("SAVEPOINT {savepoint}"))?;
            let outcome = catch_unwind(AssertUnwindSafe(|| {
                if let Some(write) = self.prepared.write.as_ref() {
                    super::super::mutation::execute_write(
                        connection,
                        write,
                        self.prepared.commit,
                        &self.prepared.params,
                        &mut self.metrics,
                        is_interrupted,
                    )
                } else if let Some(program) = self.prepared.program.as_ref() {
                    super::super::mutation::execute_program(
                        connection,
                        program,
                        self.prepared.commit,
                        &self.prepared.params,
                        &mut self.metrics,
                        is_interrupted,
                    )
                } else {
                    Err(QueryError::internal(
                        "write query is missing its mutation plan",
                    ))
                }
            }));
            let outcome = match outcome {
                Ok(Ok(outcome)) => outcome,
                Ok(Err(error)) => {
                    self.finished = true;
                    rollback_write_savepoint(connection, &savepoint)?;
                    return Err(error);
                }
                Err(_) => {
                    self.finished = true;
                    rollback_write_savepoint(connection, &savepoint)?;
                    return Err(QueryError::internal(
                        "panic while executing a Lithograph mutating query",
                    ));
                }
            };
            self.metrics.rows = outcome.rows.len().try_into().unwrap_or(u64::MAX);
            let summary = QuerySummary {
                query_type: QueryType::Write,
                commit: format!("commit/{}", outcome.commit.to_hex()),
                counters: query_counters(outcome.counters),
                metrics: self.metrics.clone(),
            };
            self.write_state = WriteState::Active {
                savepoint,
                rows: outcome.rows,
                offset: 0,
                summary,
            };
        }
        Ok(())
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

    fn next_active_write_batch(&mut self, max_rows: usize) -> QueryResult<QueryBatch> {
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
        let end = offset.saturating_add(max_rows).min(rows.len());
        let batch_rows = rows[*offset..end].to_vec();
        *offset = end;
        let done = *offset >= rows.len();
        if done {
            self.metrics.elapsed_micros =
                self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            summary.metrics = self.metrics.clone();
        }
        Ok(QueryBatch {
            rows: batch_rows,
            done,
            summary: done.then(|| summary.clone()),
        })
    }

    pub fn complete(&mut self, connection: &Connection) -> QueryResult<QuerySummary> {
        match std::mem::replace(&mut self.write_state, WriteState::None) {
            WriteState::Active {
                savepoint,
                rows,
                offset,
                mut summary,
            } => {
                if offset < rows.len() {
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
                self.metrics.elapsed_micros =
                    self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
                summary.metrics = self.metrics.clone();
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

fn rollback_write_savepoint(connection: &Connection, savepoint: &str) -> QueryResult<()> {
    let rollback_error = connection
        .execute_batch(&format!("ROLLBACK TO {savepoint}"))
        .err();
    let release_error = if rollback_error.is_none() {
        match connection.execute_batch(&format!("RELEASE {savepoint}")) {
            Ok(()) => return Ok(()),
            Err(error) => Some(error),
        }
    } else {
        None
    };
    let detail = match (rollback_error, release_error) {
        (Some(rollback), _) => format!("rollback-to-savepoint failed ({rollback})"),
        (None, Some(release)) => format!("release-savepoint failed ({release})"),
        (None, None) => "unknown cleanup failure".to_owned(),
    };
    if connection.execute_batch("ROLLBACK").is_ok() {
        return Err(QueryError::internal(format!(
            "write savepoint cleanup failed ({detail}); full SQLite rollback executed"
        )));
    }
    Err(QueryError::internal(format!(
        "write savepoint cleanup failed ({detail}); full SQLite rollback also failed"
    )))
}
