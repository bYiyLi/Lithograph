use super::*;

pub(super) struct AdapterExecution {
    cursor: query::QueryCursor,
    columns: Vec<String>,
}

impl AdapterExecution {
    pub(super) fn prepare(
        connection: &Connection,
        query_text: &str,
        params_text: &str,
        options_text: &str,
    ) -> LithographResult<Self> {
        require_initialized(connection)?;
        let params = cypher::decode_parameters_text(params_text)
            .map_err(|error| LithographError::invalid_argument(error.message))?;
        let options = query::ExecutionOptions::parse_text(options_text).map_err(map_query_error)?;
        let prepared =
            query::prepare(connection, query_text, params, options).map_err(map_query_error)?;
        let columns = prepared.columns.clone();
        Ok(Self {
            cursor: query::QueryCursor::new(prepared),
            columns,
        })
    }

    pub(super) fn columns(&self) -> &[String] {
        &self.columns
    }

    pub(super) fn next_batch(
        &mut self,
        connection: &Connection,
        max_rows: usize,
    ) -> LithographResult<query::QueryBatch> {
        // SAFETY: AdapterExecution is invoked only with the live SQLite
        // connection that owns this query. Lithograph requires SQLite >= 3.45,
        // where sqlite3_is_interrupted() is part of the loadable-extension API.
        let db = unsafe { connection.handle() };
        let is_interrupted = || host_is_interrupted(db);
        self.cursor
            .next_batch_with_interrupt(connection, max_rows, &is_interrupted)
            .map_err(map_query_error)
    }

    pub(super) fn cancel(&mut self, connection: &Connection) -> LithographResult<()> {
        self.cursor.cancel(connection).map_err(map_query_error)
    }
}

pub(super) fn scalar_result(
    connection: &Connection,
    query_text: &str,
    params_text: &str,
    options_text: &str,
) -> LithographResult<String> {
    let mut execution =
        AdapterExecution::prepare(connection, query_text, params_text, options_text)?;
    let columns = execution.columns().to_vec();
    let mut rows = Vec::new();
    let summary = loop {
        let batch = execution.next_batch(connection, 256)?;
        rows.extend(batch.rows.iter().map(|row| row_json(row)));
        if batch.done {
            break batch
                .summary
                .ok_or_else(|| LithographError::internal("completed query is missing summary"))?;
        }
    };
    Ok(json!({
        "columns": columns,
        "rows": rows,
        "summary": summary_json(&summary),
    })
    .to_string())
}

pub(super) fn row_json(row: &[cypher::Value]) -> Value {
    Value::Array(row.iter().map(cypher::encode_json).collect())
}

pub(super) fn summary_json(summary: &query::QuerySummary) -> Value {
    json!({
        "queryType": "read",
        "commit": summary.commit,
        "counters": zero_counters(),
    })
}

fn zero_counters() -> Value {
    json!({
        "nodesCreated": 0,
        "nodesDeleted": 0,
        "relationshipsCreated": 0,
        "relationshipsDeleted": 0,
        "propertiesSet": 0,
        "propertiesRemoved": 0,
        "labelsAdded": 0,
        "labelsRemoved": 0,
        "constraintsAdded": 0,
        "constraintsRemoved": 0,
        "indexesAdded": 0,
        "indexesRemoved": 0,
    })
}

pub(super) fn map_query_error(error: query::QueryError) -> LithographError {
    let category = match error.sqlite_code {
        Some(ffi::SQLITE_BUSY | ffi::SQLITE_LOCKED) => ErrorCategory::Busy,
        Some(ffi::SQLITE_IOERR | ffi::SQLITE_CANTOPEN | ffi::SQLITE_READONLY) => ErrorCategory::Io,
        _ => match error.kind {
            query::QueryErrorKind::Parse => ErrorCategory::Parse,
            query::QueryErrorKind::Semantic => ErrorCategory::Semantic,
            query::QueryErrorKind::Type => ErrorCategory::Type,
            query::QueryErrorKind::Schema => ErrorCategory::Schema,
            query::QueryErrorKind::InvalidArgument => ErrorCategory::InvalidArgument,
            query::QueryErrorKind::VersionNotFound => ErrorCategory::VersionNotFound,
            query::QueryErrorKind::BranchNotFound => ErrorCategory::BranchNotFound,
            query::QueryErrorKind::TagNotFound => ErrorCategory::TagNotFound,
            query::QueryErrorKind::Resource => ErrorCategory::Resource,
            query::QueryErrorKind::Storage => ErrorCategory::Storage,
            query::QueryErrorKind::Interrupted => ErrorCategory::Resource,
            query::QueryErrorKind::Internal => ErrorCategory::Internal,
        },
    };
    let sqlite_code = error.sqlite_code.unwrap_or(match error.kind {
        query::QueryErrorKind::Interrupted => ffi::SQLITE_INTERRUPT,
        query::QueryErrorKind::Resource => ffi::SQLITE_TOOBIG,
        _ => ffi::SQLITE_ERROR,
    });
    let message = if matches!(error.kind, query::QueryErrorKind::Storage) {
        "query storage operation failed".to_owned()
    } else {
        error.message
    };
    let mut mapped = LithographError::new(category, message, sqlite_code);
    mapped.line = error.line.map(u64::from);
    mapped.column = error.column.map(u64::from);
    mapped
}
