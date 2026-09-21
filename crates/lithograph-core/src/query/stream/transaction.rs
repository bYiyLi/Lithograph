use super::*;

impl QueryCursor {
    pub(super) fn next_transaction_program(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        self.ensure_transaction_stream(connection)?;
        if let Some(batch) =
            self.next_streaming_transaction(connection, max_rows, is_interrupted)?
        {
            return Ok(batch);
        }
        self.ensure_materialized_transaction_rows(connection, is_interrupted)?;
        self.next_materialized_transaction_batch(connection, max_rows, is_interrupted)
    }

    fn ensure_transaction_stream(&mut self, connection: &Connection) -> QueryResult<()> {
        if !self.transaction_stream_checked {
            let program = prepared_program(&self.prepared, "transaction program is missing")?;
            self.transaction_stream = crate::query::transaction::TransactionProgramStream::try_new(
                connection,
                program,
                self.prepared.commit,
                &self.prepared.params,
            )?;
            self.transaction_stream_checked = true;
        }
        Ok(())
    }

    fn next_streaming_transaction(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<Option<QueryBatch>> {
        let Some(stream) = self.transaction_stream.as_mut() else {
            return Ok(None);
        };
        let program = self.prepared.program.as_ref().ok_or_else(|| {
            QueryError::internal("transaction program disappeared during streaming")
        })?;
        let query_type = if program.writes {
            QueryType::Write
        } else {
            QueryType::Read
        };
        let batch: crate::query::transaction::TransactionStreamBatch = stream.next_batch(
            connection,
            program,
            &self.prepared.params,
            &mut self.metrics,
            max_rows,
            is_interrupted,
        )?;
        self.metrics.rows = self
            .metrics
            .rows
            .saturating_add(batch.rows.len().try_into().unwrap_or(u64::MAX));
        if batch.done {
            self.finish_streaming_transaction(query_type, &batch);
        }
        Ok(Some(QueryBatch {
            rows: batch.rows,
            done: batch.done,
            summary: batch
                .done
                .then(|| self.transaction_summary.clone())
                .flatten(),
        }))
    }

    fn finish_streaming_transaction(
        &mut self,
        query_type: QueryType,
        batch: &crate::query::transaction::TransactionStreamBatch,
    ) {
        self.metrics.elapsed_micros =
            self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
        self.metrics.finish_profile();
        self.finished = true;
        self.transaction_summary = Some(committed_summary(
            query_type,
            batch.commit,
            batch.counters.clone(),
            &self.metrics,
            self.suppress_summary_commit,
        ));
    }

    fn ensure_materialized_transaction_rows(
        &mut self,
        connection: &Connection,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<()> {
        if self.transaction_rows.is_none() {
            let program = prepared_program(&self.prepared, "transaction program is missing")?;
            let outcome = crate::query::transaction::execute_transaction_program(
                connection,
                program,
                self.prepared.commit,
                &self.prepared.params,
                &mut self.metrics,
                is_interrupted,
            )?;
            self.metrics.rows = outcome
                .rows
                .as_ref()
                .map_or(0, |rows| rows.total().try_into().unwrap_or(u64::MAX));
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
            self.transaction_rows = outcome.rows;
        }
        Ok(())
    }

    fn next_materialized_transaction_batch(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<QueryBatch> {
        let (batch_rows, done) =
            self.take_transaction_rows(connection, max_rows, is_interrupted)?;
        if done {
            self.metrics.elapsed_micros =
                self.started.elapsed().as_micros().min(u128::from(u64::MAX)) as u64;
            self.metrics.finish_profile();
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

    fn take_transaction_rows(
        &mut self,
        connection: &Connection,
        max_rows: usize,
        is_interrupted: &dyn Fn() -> bool,
    ) -> QueryResult<(Vec<Vec<Value>>, bool)> {
        let Some(rows) = self.transaction_rows.as_mut() else {
            return Ok((Vec::new(), true));
        };
        let batch_rows = rows.next_batch(connection, max_rows, is_interrupted)?;
        self.transaction_offset = self.transaction_offset.saturating_add(batch_rows.len());
        Ok((batch_rows, self.transaction_offset >= rows.total()))
    }
}
