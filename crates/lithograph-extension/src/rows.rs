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
    data: String,
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
    pending_rows: VecDeque<String>,
    pending_summary: Option<query::QuerySummary>,
    ordinal: i64,
    execution_done: bool,
    phantom: PhantomData<&'vtab RowsTab>,
}

impl RowsCursor<'_> {
    fn emit_row(&mut self, row: String) {
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
            let batch = execution.next_batch(connection, ROWS_PREFETCH)?;
            self.pending_rows.extend(
                batch
                    .rows
                    .iter()
                    .map(|row| execution::row_json(row).to_string()),
            );
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
        let data = execution::summary_json(&candidate).to_string();
        execution::ensure_scalar_result_fits(connection, &data)?;
        let execution = self.execution.as_mut().ok_or_else(|| {
            LithographError::internal("lithograph_rows execution disappeared before summary")
        })?;
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

struct RowsFilterInput {
    query: String,
    params: String,
    options: String,
}

fn parse_rows_filter_input(
    idx_num: c_int,
    args: &Filters<'_>,
) -> LithographResult<RowsFilterInput> {
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

fn required_filter_text(
    present: bool,
    args: &Filters<'_>,
    index: &mut usize,
    name: &str,
) -> LithographResult<String> {
    if !present {
        return Err(LithographError::invalid_argument(format!(
            "{name} is required"
        )));
    }
    let value = args
        .get::<String>(*index)
        .map_err(|_| LithographError::invalid_argument(format!("{name} must be TEXT")))?;
    *index += 1;
    Ok(value)
}

fn optional_filter_text(
    present: bool,
    args: &Filters<'_>,
    index: &mut usize,
    name: &str,
) -> LithographResult<String> {
    if present {
        required_filter_text(true, args, index, name)
    } else {
        Ok("{}".to_owned())
    }
}

impl RowsCursor<'_> {
    fn apply_rows_filter(&mut self, input: RowsFilterInput) -> SqliteResult<()> {
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
        let execution = execution::AdapterExecution::prepare(
            &connection,
            &input.query,
            &input.params,
            &input.options,
        )
        .map_err(|error| error.to_sqlite_error())?;
        let columns_data = serde_json::to_string(execution.columns()).map_err(|error| {
            LithographError::internal(format!("failed to encode result columns: {error}"))
                .to_sqlite_error()
        })?;
        execution::ensure_scalar_result_fits(&connection, &columns_data)
            .map_err(|error| error.to_sqlite_error())?;

        self.query = input.query;
        self.params = input.params;
        self.options = input.options;
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
            self.advance(&connection)
                .map_err(|error| error.to_sqlite_error())
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
                2 => ctx.set_result(&current.data),
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
