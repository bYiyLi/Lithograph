use rusqlite::Connection;

use super::*;
use crate::query::resources::ExecutionControl;

impl QueryCursor {
    pub(super) fn next_program_read(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        let resources = self.prepared.resources.clone();
        let control = ExecutionControl::new(&resources, is_interrupted);
        self.ensure_program_stream(connection, &control)?;
        if self.program_stream.is_some() {
            return self.next_program_stream(connection, max_rows, &control);
        }
        if matches!(self.barrier, BarrierState::Ready(_)) && self.spill_connection.is_some() {
            return self.next_spilled(max_rows, is_interrupted);
        }
        self.materialize_program_fallback(connection, &control)?;
        self.next_spilled(max_rows, is_interrupted)
    }

    fn ensure_program_stream(
        &mut self,
        connection: &Connection,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        if self.program_stream_checked {
            return Ok(());
        }
        let snapshot = prepared_snapshot(connection, &self.prepared, control)?;
        let program = prepared_program(&self.prepared, "Phase 06 program is missing")?;
        self.program_stream = program::build_program_stream(
            connection,
            program,
            &snapshot,
            self.prepared.mode,
            &self.prepared.params,
            control,
        )?;
        self.program_stream_checked = true;
        Ok(())
    }

    fn next_program_stream(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<QueryBatch> {
        let snapshot = prepared_snapshot(connection, &self.prepared, control)?;
        let mut memory = control.reserve(
            crate::query::resources::allocation_bytes::<Vec<Value>>(max_rows)?,
        )?;
        let mut rows = Vec::with_capacity(max_rows);
        while rows.len() < max_rows {
            control.check()?;
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
                    control,
                )?;
            let Some(row) = next else {
                self.metrics.rows = self
                    .metrics
                    .rows
                    .saturating_add(rows.len().try_into().unwrap_or(u64::MAX));
                self.batch_memory = Some(memory);
                return self.finish(rows);
            };
            if let Some(output_memory) = self
                .program_stream
                .as_mut()
                .and_then(program::ProgramStream::take_output_memory)
            {
                memory.merge(output_memory)?;
            }
            rows.push(row);
        }
        self.metrics.rows = self
            .metrics
            .rows
            .saturating_add(rows.len().try_into().unwrap_or(u64::MAX));
        self.batch_memory = Some(memory);
        Ok(QueryBatch {
            _memory: None,
            rows,
            done: false,
            summary: None,
        })
    }

    fn materialize_program_fallback(
        &mut self,
        connection: &Connection,
        control: &ExecutionControl<'_>,
    ) -> QueryResult<()> {
        let snapshot = prepared_snapshot(connection, &self.prepared, control)?;
        let program = prepared_program(&self.prepared, "Phase 06 program is missing")?;
        let (rows, summary_commit, _memory) =
            super::super::completeness::execute_prepared_read_with_summary_commit(
                connection,
                program,
                snapshot,
                &self.prepared.graph_view,
                &self.prepared.params,
                self.prepared.mode,
                &mut self.metrics,
                control,
            )?;
        self.read_summary_commit = summary_commit;
        self.metrics.rows = rows.len().try_into().unwrap_or(u64::MAX);
        let spill_connection = open_spill_connection()?;
        let mut spill =
            RowSpill::create_with_resources(&spill_connection, control.resources().clone())?;
        spill.push_all(&spill_connection, &rows, control)?;
        self.barrier = BarrierState::Ready(spill.output());
        self.spill_connection = Some(spill_connection);
        Ok(())
    }
}
