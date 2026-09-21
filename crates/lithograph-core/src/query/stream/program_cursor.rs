use rusqlite::Connection;

use super::*;

impl QueryCursor {
    pub(super) fn next_program_read(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        self.ensure_program_stream(connection)?;
        if self.program_stream.is_some() {
            return self.next_program_stream(connection, max_rows, is_interrupted);
        }
        if matches!(self.barrier, BarrierState::Ready(_)) && self.spill_connection.is_some() {
            return self.next_spilled(max_rows, is_interrupted);
        }
        self.materialize_program_fallback(connection, is_interrupted)?;
        self.next_spilled(max_rows, is_interrupted)
    }

    fn ensure_program_stream(&mut self, connection: &Connection) -> QueryResult<()> {
        if self.program_stream_checked {
            return Ok(());
        }
        let snapshot = prepared_snapshot(connection, &self.prepared)?;
        let program = prepared_program(&self.prepared, "Phase 06 program is missing")?;
        self.program_stream = program::build_program_stream(
            connection,
            program,
            &snapshot,
            self.prepared.mode,
            &self.prepared.params,
        )?;
        self.program_stream_checked = true;
        Ok(())
    }

    fn next_program_stream(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        let snapshot = prepared_snapshot(connection, &self.prepared)?;
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            check_interrupted(is_interrupted)?;
            let next = self
                .program_stream
                .as_mut()
                .ok_or_else(|| QueryError::internal("program stream disappeared"))?
                .next(
                    connection,
                    &snapshot,
                    &self.prepared.graph_view,
                    &self.prepared.params,
                    &mut self.metrics,
                    is_interrupted,
                )?;
            let Some(row) = next else {
                self.metrics.rows = self
                    .metrics
                    .rows
                    .saturating_add(rows.len().try_into().unwrap_or(u64::MAX));
                return self.finish(rows);
            };
            rows.push(row);
        }
        self.metrics.rows = self
            .metrics
            .rows
            .saturating_add(rows.len().try_into().unwrap_or(u64::MAX));
        Ok(QueryBatch {
            rows,
            done: false,
            summary: None,
        })
    }

    fn materialize_program_fallback(
        &mut self,
        connection: &Connection,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        let snapshot = prepared_snapshot(connection, &self.prepared)?;
        let program = prepared_program(&self.prepared, "Phase 06 program is missing")?;
        let (rows, summary_commit) =
            super::super::completeness::execute_prepared_read_with_summary_commit(
                connection,
                program,
                snapshot,
                &self.prepared.graph_view,
                &self.prepared.params,
                self.prepared.mode,
                &mut self.metrics,
                is_interrupted,
            )?;
        self.read_summary_commit = summary_commit;
        self.metrics.rows = rows.len().try_into().unwrap_or(u64::MAX);
        let spill_connection = open_spill_connection()?;
        let mut spill = RowSpill::create(&spill_connection)?;
        spill.push_all(&spill_connection, &rows)?;
        self.barrier = BarrierState::Ready(spill.output());
        self.spill_connection = Some(spill_connection);
        Ok(())
    }
}
