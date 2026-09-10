use super::*;

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
                exhausted: true,
                phantom: PhantomData,
            })
        })
    }
}

#[repr(C)]
pub(super) struct RowsCursor<'vtab> {
    base: ffi::sqlite3_vtab_cursor,
    db: *mut ffi::sqlite3,
    exhausted: bool,
    phantom: PhantomData<&'vtab RowsTab>,
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
            cypher::decode_parameters_text(&params).map_err(|error| {
                LithographError::invalid_argument(error.message).to_sqlite_error()
            })?;
            validate_json_object(&options, "options").map_err(|e| e.to_sqlite_error())?;
            // SAFETY: `self.db` was captured from the live VTab connection and
            // SQLite invokes this cursor only while that connection is valid.
            let connection = unsafe { Connection::from_handle(self.db) }.map_err(|error| {
                map_sqlite_error(error, "failed to access the SQLite connection").to_sqlite_error()
            })?;
            require_initialized(&connection).map_err(|e| e.to_sqlite_error())?;
            validate_cypher(&query).map_err(|e| e.to_sqlite_error())?;

            self.exhausted = true;
            Err(LithographError::execution_unavailable().to_sqlite_error())
        })
    }

    fn next(&mut self) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            self.exhausted = true;
            Ok(())
        })
    }

    fn eof(&self) -> bool {
        self.exhausted
    }

    fn column(&self, _ctx: &mut VTabContext, _i: c_int) -> SqliteResult<()> {
        catch_sqlite_boundary(|| {
            Err(
                LithographError::internal("lithograph_rows cursor has no current row")
                    .to_sqlite_error(),
            )
        })
    }

    fn rowid(&self) -> SqliteResult<i64> {
        catch_sqlite_boundary(|| {
            Err(
                LithographError::internal("lithograph_rows cursor has no current row")
                    .to_sqlite_error(),
            )
        })
    }
}
