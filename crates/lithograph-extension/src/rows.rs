use super::*;
use std::collections::VecDeque;

const ROWS_PREFETCH: usize = 256;

#[repr(C)]
pub(super) struct RowsTab {
    base: ffi::sqlite3_vtab,
    db: *mut ffi::sqlite3,
}

// SAFETY: RowsTab is repr(C) and stores sqlite3_vtab as its first field,
// matching the layout contract required by rusqlite's VTab trait.
unsafe impl<'vtab> VTab<'vtab> for RowsTab {
    type Aux = ConnectionRegistration;
    type Cursor = RowsCursor<'vtab>;

    fn connect(
        db: &mut VTabConnection,
        _aux: Option<&Self::Aux>,
        _module_name: &[u8],
        _database_name: &[u8],
        _table_name: &[u8],
        _args: &[&[u8]],
    ) -> SqliteResult<(Cow<'static, CStr>, Self)> {
        catch_sqlite_boundary(|| {
            db.config(VTabConfig::DirectOnly)?;
            // SAFETY: db is the live VTab connection supplied by SQLite and
            // the handle is retained only for callbacks on this same cursor.
            let handle = unsafe { db.handle() };
            Ok((
                Cow::Borrowed(c"CREATE TABLE x(ordinal INTEGER, event TEXT, data TEXT, query HIDDEN, params HIDDEN, options HIDDEN)"),
                Self {
                    base: ffi::sqlite3_vtab::default(),
                    db: handle,
                },
            ))
        })
    }

    fn best_index(&self, info: &mut IndexInfo) -> SqliteResult<bool> {
        catch_sqlite_boundary(|| {
            const QUERY_COLUMN: c_int = 3;
            const PARAMS_COLUMN: c_int = 4;
            const OPTIONS_COLUMN: c_int = 5;

            let mut selected = [None, None, None];
            for (index, constraint) in info.constraints().enumerate() {
                let slot = match constraint.column() {
                    QUERY_COLUMN => Some(0),
                    PARAMS_COLUMN => Some(1),
                    OPTIONS_COLUMN => Some(2),
                    _ => None,
                };
                let Some(slot) = slot else {
                    continue;
                };
                if constraint.is_usable()
                    && constraint.operator() == IndexConstraintOp::SQLITE_INDEX_CONSTRAINT_EQ
                {
                    selected[slot] = Some(index);
                }
            }

            let mut argv = 1;
            let mut idx_num = 0;
            for (slot, constraint_index) in selected.into_iter().enumerate() {
                if let Some(constraint_index) = constraint_index {
                    let mut usage = info.constraint_usage(constraint_index);
                    usage.set_argv_index(argv);
                    usage.set_omit(true);
                    argv += 1;
                    idx_num |= 1 << slot;
                }
            }
            info.set_idx_num(idx_num);
            if selected[0].is_some() {
                info.set_estimated_cost(10.0);
                info.set_estimated_rows(1000);
            } else {
                info.set_estimated_cost(1_000_000_000.0);
                info.set_estimated_rows(1);
            }
            Ok(true)
        })
    }

    fn open(&'vtab mut self) -> SqliteResult<Self::Cursor> {
        catch_sqlite_boundary(|| {
            Ok(RowsCursor {
                base: ffi::sqlite3_vtab_cursor::default(),
                db: self.db,
                execution: None,
                query: String::new(),
                params: String::new(),
                options: String::new(),
                current_event: None,
                pending_rows: VecDeque::new(),
                pending_memory: None,
                input_memory: None,
                pending_summary: None,
                ordinal: 0,
                execution_done: false,
                phantom: PhantomData,
            })
        })
    }
}

struct RowsEvent {
    kind: &'static str,
    data: budget::BudgetedString,
}

#[repr(C)]
pub(super) struct RowsCursor<'vtab> {
    base: ffi::sqlite3_vtab_cursor,
    db: *mut ffi::sqlite3,
    execution: Option<execution::AdapterExecution>,
    query: String,
    params: String,
    options: String,
    current_event: Option<RowsEvent>,
    pending_rows: VecDeque<budget::BudgetedString>,
    pending_memory: Option<query::MemoryReservation>,
    input_memory: Option<query::MemoryReservation>,
    pending_summary: Option<query::QuerySummary>,
    ordinal: i64,
    execution_done: bool,
    phantom: PhantomData<&'vtab RowsTab>,
}

impl RowsCursor<'_> {
    fn emit_row(&mut self, row: budget::BudgetedString) {
        self.ordinal = self.ordinal.saturating_add(1);
        self.current_event = Some(RowsEvent {
            kind: "row",
            data: row,
        });
    }

    fn advance(&mut self, connection: &Connection) -> LithographResult<()> {
        self.current_event = None;
        if let Some(row) = self.pending_rows.pop_front() {
            self.emit_row(row);
            return Ok(());
        }
        if self.execution_done {
            return self.finish_summary(connection);
        }

        loop {
            let Some(execution) = self.execution.as_mut() else {
                return Ok(());
            };
            // Under an explicit budget, avoid retaining a Core batch and a
            // second full batch of serialized payloads at the same time.
            let prefetch = if execution.resources().limit().is_some() {
                1
            } else {
                ROWS_PREFETCH
            };
            let batch = execution.next_batch(connection, prefetch)?;
            encode_pending_rows(
                connection,
                self.db,
                execution,
                &batch.rows,
                &mut self.pending_rows,
                &mut self.pending_memory,
            )?;
            if batch.done {
                self.execution_done = true;
                self.pending_summary = batch.summary;
            }
            if let Some(row) = self.pending_rows.pop_front() {
                self.emit_row(row);
                return Ok(());
            }
            if self.execution_done {
                return self.finish_summary(connection);
            }
        }
    }

    fn finish_summary(&mut self, connection: &Connection) -> LithographResult<()> {
        let candidate = self.pending_summary.take().ok_or_else(|| {
            LithographError::internal("lithograph_rows terminal batch is missing summary")
        })?;
        self.pending_rows = VecDeque::new();
        self.pending_memory = None;
        let execution = self.execution.as_mut().ok_or_else(|| {
            LithographError::internal("lithograph_rows execution disappeared before summary")
        })?;
        let metadata_bytes = execution::adapter_metadata_bytes(&[], &candidate)?;
        let summary = budget::json_owned(execution.resources(), metadata_bytes, || {
            execution::summary_json(&candidate)
        })?;
        let data = budget::serialize_json(&summary.value, execution.resources(), &|| {
            host_is_interrupted(self.db)
        })?;
        execution::ensure_scalar_result_fits(connection, &data.text)?;
        let _completed = execution.complete(connection)?;
        self.execution = None;
        self.ordinal = self.ordinal.saturating_add(1);
        self.current_event = Some(RowsEvent {
            kind: "summary",
            data,
        });
        Ok(())
    }
}

fn encode_pending_rows(
    connection: &Connection,
    db: *mut ffi::sqlite3,
    execution: &execution::AdapterExecution,
    rows: &[Vec<cypher::Value>],
    pending: &mut VecDeque<budget::BudgetedString>,
    memory: &mut Option<query::MemoryReservation>,
) -> LithographResult<()> {
    for row in rows {
        let json = execution.budgeted_row_json(row, &|| host_is_interrupted(db))?;
        let encoded = budget::serialize_json(&json.value, execution.resources(), &|| {
            host_is_interrupted(db)
        })?;
        execution::ensure_scalar_result_fits(connection, &encoded.text)?;
        reserve_pending_slot(pending, memory, execution.resources())?;
        pending.push_back(encoded);
    }
    Ok(())
}

fn reserve_pending_slot(
    pending: &mut VecDeque<budget::BudgetedString>,
    memory: &mut Option<query::MemoryReservation>,
    resources: &query::QueryResources,
) -> LithographResult<()> {
    if pending.len() < pending.capacity() {
        return Ok(());
    }
    let previous = query::resources::allocation_bytes::<budget::BudgetedString>(pending.capacity())
        .map_err(execution::map_query_error)?;
    let _growth_memory = resources
        .reserve(previous)
        .map_err(execution::map_query_error)?;
    let additional = pending.capacity().max(4);
    let bytes = query::resources::allocation_bytes::<budget::BudgetedString>(additional)
        .map_err(execution::map_query_error)?;
    match memory.as_mut() {
        Some(memory) => memory.grow(bytes).map_err(execution::map_query_error)?,
        None => {
            *memory = Some(
                resources
                    .reserve(bytes)
                    .map_err(execution::map_query_error)?,
            )
        }
    }
    pending.reserve_exact(additional);
    Ok(())
}

impl Drop for RowsCursor<'_> {
    fn drop(&mut self) {
        let Some(mut execution) = self.execution.take() else {
            return;
        };
        // SAFETY: SQLite invokes xClose while the owning VTab connection is live;
        // from_handle borrows the raw handle and never closes it on drop.
        if let Ok(connection) = unsafe { Connection::from_handle(self.db) }
            && execution.cancel(&connection).is_err()
        {
            quarantine_connection(&connection);
        }
    }
}

struct RowsFilterInput<'a> {
    query: &'a str,
    params: &'a str,
    options: &'a str,
}

fn parse_rows_filter_input<'a>(
    idx_num: c_int,
    args: &'a Filters<'_>,
) -> LithographResult<RowsFilterInput<'a>> {
    let mut index = 0;
    let query = required_filter_text(idx_num & 1 != 0, args, &mut index, "query")?;
    let params = optional_filter_text(idx_num & 2 != 0, args, &mut index, "params")?;
    let options = optional_filter_text(idx_num & 4 != 0, args, &mut index, "options")?;
    if query.trim().is_empty() {
        return Err(LithographError::invalid_argument("query must not be empty"));
    }
    Ok(RowsFilterInput {
        query,
        params,
        options,
    })
}

fn required_filter_text<'a>(
    present: bool,
    args: &'a Filters<'_>,
    index: &mut usize,
    name: &str,
) -> LithographResult<&'a str> {
    if !present {
        return Err(LithographError::invalid_argument(format!(
            "{name} is required"
        )));
    }
    let value = args
        .iter()
        .nth(*index)
        .ok_or_else(|| LithographError::internal("missing rows filter argument"))?
        .as_str()
        .map_err(|_| LithographError::invalid_argument(format!("{name} must be TEXT")))?;
    *index += 1;
    Ok(value)
}

fn optional_filter_text<'a>(
    present: bool,
    args: &'a Filters<'_>,
    index: &mut usize,
    name: &str,
) -> LithographResult<&'a str> {
    if present {
        required_filter_text(true, args, index, name)
    } else {
        Ok("{}")
    }
}

impl RowsCursor<'_> {
    fn apply_rows_filter(&mut self, input: RowsFilterInput<'_>) -> SqliteResult<()> {
        // SAFETY: self.db belongs to the live VTab connection for this cursor.
        let connection = unsafe { Connection::from_handle(self.db) }.map_err(|error| {
            map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
        })?;
        require_connection_healthy(&connection).map_err(|error| error.to_sqlite_error())?;
        if let Some(mut previous) = self.execution.take() {
            previous
                .cancel(&connection)
                .map_err(|error| error.to_sqlite_error())?;
        }
        self.current_event = None;
        self.pending_rows = VecDeque::new();
        self.pending_memory = None;
        self.query = String::new();
        self.params = String::new();
        self.options = String::new();
        self.input_memory = None;
        self.pending_summary = None;
        let mut execution = execution::AdapterExecution::prepare(
            &connection,
            input.query,
            input.params,
            input.options,
        )
        .map_err(|error| error.to_sqlite_error())?;
        let columns_candidate = prepare_rows_delivery(&connection, self.db, &execution, &input);
        let (columns_data, query_text, params_text, options_text, inputs_memory) =
            match columns_candidate {
                Ok(candidate) => candidate,
                Err(error) => {
                    return match execution.cancel(&connection) {
                        Ok(()) => Err(error.to_sqlite_error()),
                        Err(cleanup) => Err(cleanup.to_sqlite_error()),
                    };
                }
            };

        self.query = query_text;
        self.params = params_text;
        self.options = options_text;
        self.input_memory = Some(inputs_memory);
        self.execution = Some(execution);
        self.current_event = Some(RowsEvent {
            kind: "columns",
            data: columns_data,
        });
        self.pending_rows.clear();
        self.pending_summary = None;
        self.ordinal = 0;
        self.execution_done = false;
        Ok(())
    }
}

fn copy_rows_inputs(
    input: &RowsFilterInput<'_>,
    resources: &query::QueryResources,
    is_interrupted: &dyn Fn() -> bool,
) -> LithographResult<(String, String, String, query::MemoryReservation)> {
    let bytes = [input.query, input.params, input.options]
        .iter()
        .try_fold(0_u64, |bytes, text| {
            query::resources::checked_add(bytes, text.len() as u64)
        })
        .map_err(execution::map_query_error)?;
    let memory = resources
        .reserve(bytes)
        .map_err(execution::map_query_error)?;
    let control = query::ExecutionControl::new(resources, is_interrupted);
    Ok((
        budget::copy_text(input.query, &control)?,
        budget::copy_text(input.params, &control)?,
        budget::copy_text(input.options, &control)?,
        memory,
    ))
}

fn prepare_columns(
    connection: &Connection,
    db: *mut ffi::sqlite3,
    execution: &execution::AdapterExecution,
) -> LithographResult<budget::BudgetedString> {
    let columns_bytes = execution
        .columns()
        .iter()
        .try_fold(256_u64, |bytes, column| {
            query::resources::checked_add(bytes, column.len() as u64 + 256)
        })
        .map_err(execution::map_query_error)?;
    let columns = budget::json_owned(execution.resources(), columns_bytes, || {
        json!(execution.columns())
    })?;
    let data = budget::serialize_json(&columns.value, execution.resources(), &|| {
        host_is_interrupted(db)
    })?;
    execution::ensure_scalar_result_fits(connection, &data.text)?;
    Ok(data)
}

fn prepare_rows_delivery(
    connection: &Connection,
    db: *mut ffi::sqlite3,
    execution: &execution::AdapterExecution,
    input: &RowsFilterInput<'_>,
) -> LithographResult<(
    budget::BudgetedString,
    String,
    String,
    String,
    query::MemoryReservation,
)> {
    let (query_text, params_text, options_text, memory) =
        copy_rows_inputs(input, execution.resources(), &|| host_is_interrupted(db))?;
    let columns = prepare_columns(connection, db, execution)?;
    Ok((columns, query_text, params_text, options_text, memory))
}

// SAFETY: RowsCursor is repr(C) and stores sqlite3_vtab_cursor first, as
// required by rusqlite; its raw database handle comes from its owning VTab.
unsafe impl VTabCursor for RowsCursor<'_> {
    fn filter(
        &mut self,
        idx_num: c_int,
        _idx_str: Option<&str>,
        args: &Filters<'_>,
    ) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            let input = match parse_rows_filter_input(idx_num, args) {
                Ok(input) => input,
                Err(error) => {
                    // SAFETY: the cursor retains the live VTab database handle.
                    let connection =
                        unsafe { Connection::from_handle(self.db) }.map_err(|sqlite_error| {
                            map_sqlite_error(sqlite_error, "failed to access the SQLite connection")
                                .to_sqlite_error()
                        })?;
                    return transaction::fail_closed_sql_transaction::<()>(&connection, error)
                        .map_err(|error| error.to_sqlite_error());
                }
            };
            self.apply_rows_filter(input)
        })
    }

    fn next(&mut self) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            if self
                .current_event
                .as_ref()
                .is_some_and(|event| event.kind == "summary")
            {
                self.current_event = None;
                return Ok(());
            }
            // SAFETY: the cursor retains the live VTab database handle.
            let connection = unsafe { Connection::from_handle(self.db) }.map_err(|error| {
                map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
            })?;
            require_connection_healthy(&connection).map_err(|error| error.to_sqlite_error())?;
            if let Err(error) = self.advance(&connection) {
                self.current_event = None;
                self.pending_rows = VecDeque::new();
                self.pending_memory = None;
                self.pending_summary = None;
                if let Some(mut execution) = self.execution.take()
                    && let Err(cleanup) = execution.cancel(&connection)
                {
                    return Err(cleanup.to_sqlite_error());
                }
                return Err(error.to_sqlite_error());
            }
            Ok(())
        })
    }

    fn eof(&self) -> bool {
        self.current_event.is_none()
    }

    fn column(&self, ctx: &mut VTabContext, i: c_int) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            let current = self.current_event.as_ref().ok_or_else(|| {
                LithographError::internal("lithograph_rows cursor has no current event")
                    .to_sqlite_error()
            })?;
            match i {
                0 => ctx.set_result(&self.ordinal),
                1 => ctx.set_result(&current.kind),
                2 => ctx.set_result(&current.data.text),
                3 => ctx.set_result(&self.query),
                4 => ctx.set_result(&self.params),
                5 => ctx.set_result(&self.options),
                _ => Err(SqliteError::InvalidColumnIndex(
                    usize::try_from(i).unwrap_or(usize::MAX),
                )),
            }
        })
    }

    fn rowid(&self) -> SqliteResult<i64> {
        catch_sqlite_boundary(|| {
            if self.current_event.is_none() {
                return Err(LithographError::internal(
                    "lithograph_rows cursor has no current event",
                )
                .to_sqlite_error());
            }
            Ok(self.ordinal)
        })
    }
}
