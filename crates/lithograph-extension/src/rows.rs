use super::*;
use std::collections::VecDeque;

const ROWS_PREFETCH: usize = 256;

#[repr(C)]
pub(super) struct RowsTab {
    base: ffi::sqlite3_vtab,
    db: *mut ffi::sqlite3,
}

// SAFETY: `RowsTab` is `repr(C)` and stores `sqlite3_vtab` as its first field,
// matching the layout contract required by rusqlite's `VTab` trait.
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
            // SAFETY: `db` is the live VTab connection supplied by SQLite and
            // the handle is retained only for callbacks on this same cursor.
            let handle = unsafe { db.handle() };
            Ok((
                Cow::Borrowed(c"CREATE TABLE x(ordinal INTEGER, columns TEXT, row TEXT, query HIDDEN, params HIDDEN, options HIDDEN)"),
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
                columns_json: "[]".to_owned(),
                query: String::new(),
                params: String::new(),
                options: String::new(),
                current_row: None,
                pending_rows: VecDeque::new(),
                ordinal: 0,
                execution_done: false,
                phantom: PhantomData,
            })
        })
    }
}

#[repr(C)]
pub(super) struct RowsCursor<'vtab> {
    base: ffi::sqlite3_vtab_cursor,
    db: *mut ffi::sqlite3,
    execution: Option<execution::AdapterExecution>,
    columns_json: String,
    query: String,
    params: String,
    options: String,
    current_row: Option<String>,
    pending_rows: VecDeque<String>,
    ordinal: i64,
    execution_done: bool,
    phantom: PhantomData<&'vtab RowsTab>,
}

impl RowsCursor<'_> {
    fn advance(&mut self, connection: &Connection) -> LithographResult<()> {
        self.current_row = None;
        if let Some(row) = self.pending_rows.pop_front() {
            self.current_row = Some(row);
            return Ok(());
        }
        if self.execution_done {
            self.execution = None;
            return Ok(());
        }
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
        self.execution_done = batch.done;
        self.current_row = self.pending_rows.pop_front();
        if self.current_row.is_none() && self.execution_done {
            self.execution = None;
        }
        Ok(())
    }
}

impl Drop for RowsCursor<'_> {
    fn drop(&mut self) {
        let Some(mut execution) = self.execution.take() else {
            return;
        };
        // SAFETY: SQLite invokes xClose while the owning VTab connection is live;
        // `from_handle` borrows the raw handle and never closes it on drop.
        if let Ok(connection) = unsafe { Connection::from_handle(self.db) } {
            let _ = execution.cancel(&connection);
        }
    }
}

// SAFETY: `RowsCursor` is `repr(C)` and stores `sqlite3_vtab_cursor` first, as
// required by rusqlite; its raw database handle comes from its owning VTab.
unsafe impl VTabCursor for RowsCursor<'_> {
    fn filter(
        &mut self,
        idx_num: c_int,
        _idx_str: Option<&str>,
        args: &Filters<'_>,
    ) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            let mut index = 0;
            let query = if idx_num & 1 != 0 {
                let value = args.get::<String>(index).map_err(|_| {
                    LithographError::invalid_argument("query must be TEXT").to_sqlite_error()
                })?;
                index += 1;
                value
            } else {
                return Err(
                    LithographError::invalid_argument("query is required").to_sqlite_error()
                );
            };
            let params = if idx_num & 2 != 0 {
                let value = args.get::<String>(index).map_err(|_| {
                    LithographError::invalid_argument("params must be JSON TEXT").to_sqlite_error()
                })?;
                index += 1;
                value
            } else {
                "{}".to_owned()
            };
            let options = if idx_num & 4 != 0 {
                args.get::<String>(index).map_err(|_| {
                    LithographError::invalid_argument("options must be JSON TEXT").to_sqlite_error()
                })?
            } else {
                "{}".to_owned()
            };

            if query.trim().is_empty() {
                return Err(
                    LithographError::invalid_argument("query must not be empty").to_sqlite_error()
                );
            }
            // SAFETY: `self.db` was captured from the live VTab connection and
            // SQLite invokes this cursor only while that connection is valid.
            let connection = unsafe { Connection::from_handle(self.db) }.map_err(|error| {
                map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
            })?;
            if let Some(mut previous) = self.execution.take() {
                previous
                    .cancel(&connection)
                    .map_err(|e| e.to_sqlite_error())?;
            }
            let execution =
                execution::AdapterExecution::prepare(&connection, &query, &params, &options)
                    .map_err(|e| e.to_sqlite_error())?;
            if execution.is_write() {
                return Err(execution::map_query_error(query::QueryError::read_only_adapter(
                    "lithograph_rows is a read-only adapter and does not execute mutating Cypher",
                ))
                .to_sqlite_error());
            }
            self.columns_json = serde_json::to_string(execution.columns()).map_err(|error| {
                LithographError::internal(format!("failed to encode result columns: {error}"))
                    .to_sqlite_error()
            })?;
            self.query = query;
            self.params = params;
            self.options = options;
            self.execution = Some(execution);
            self.current_row = None;
            self.pending_rows.clear();
            self.ordinal = 0;
            self.execution_done = false;
            self.advance(&connection).map_err(|e| e.to_sqlite_error())
        })
    }

    fn next(&mut self) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            if self.current_row.is_some() {
                self.ordinal = self.ordinal.saturating_add(1);
            }
            // SAFETY: the cursor retains the live VTab database handle.
            let connection = unsafe { Connection::from_handle(self.db) }.map_err(|error| {
                map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
            })?;
            self.advance(&connection).map_err(|e| e.to_sqlite_error())
        })
    }

    fn eof(&self) -> bool {
        self.current_row.is_none()
    }

    fn column(&self, ctx: &mut VTabContext, i: c_int) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            let current = self.current_row.as_ref().ok_or_else(|| {
                LithographError::internal("lithograph_rows cursor has no current row")
                    .to_sqlite_error()
            })?;
            match i {
                0 => ctx.set_result(&self.ordinal),
                1 => ctx.set_result(&self.columns_json),
                2 => ctx.set_result(current),
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
            if self.current_row.is_none() {
                return Err(
                    LithographError::internal("lithograph_rows cursor has no current row")
                        .to_sqlite_error(),
                );
            }
            Ok(self.ordinal)
        })
    }
}
