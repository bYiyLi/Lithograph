//! Recover a real SQLite interrupt while the outer scalar/vtab VM is active.
//! SQL cleanup cannot be prepared in that window. A writable incremental-blob
//! open creates a SQLite write VM through the public API, whose INTERRUPT halt
//! rolls back the transaction. The dedicated TEMP table is always empty.

use super::*;

const TABLE: &CStr = c"_lithograph_interrupt_guard";

pub(super) fn prepare(connection: &Connection) -> LithographResult<()> {
    connection
        .execute_batch("CREATE TEMP TABLE IF NOT EXISTS _lithograph_interrupt_guard(payload BLOB)")
        .map_err(map_prepare_error)?;
    // IF NOT EXISTS also accepts a same-named view or an incompatible table.
    // Reject those before any graph write or explicit BEGIN: blob_open must
    // be able to create its writable VM when recovery is eventually needed.
    let rowid_table = connection
        .query_row(
            "PRAGMA temp.table_list('_lithograph_interrupt_guard')",
            [],
            |row| {
                Ok(row.get::<_, String>(2)? == "table"
                    && row.get::<_, i64>(3)? == 1
                    && row.get::<_, i64>(4)? == 0)
            },
        )
        .optional()
        .map_err(map_prepare_error)?
        .unwrap_or(false);
    if !rowid_table {
        return Err(incompatible_guard());
    }
    let compatible = connection
        .query_row(
            "SELECT EXISTS(SELECT 1 FROM pragma_table_xinfo(?1,'temp') \
             WHERE cid=0 AND name='payload' COLLATE NOCASE \
             AND type='BLOB' COLLATE NOCASE AND pk=0 AND hidden=0) \
             AND NOT EXISTS(SELECT 1 FROM pragma_index_list(?1,'temp')) \
             AND NOT EXISTS(SELECT 1 FROM pragma_foreign_key_list(?1,'temp')) \
             AND NOT EXISTS(SELECT 1 FROM temp.sqlite_schema \
             WHERE type='trigger' AND tbl_name=?1 COLLATE NOCASE)",
            ["_lithograph_interrupt_guard"],
            |row| row.get::<_, bool>(0),
        )
        .map_err(map_prepare_error)?;
    if !compatible {
        return Err(incompatible_guard());
    }
    let empty = connection
        .query_row(
            "SELECT NOT EXISTS(SELECT 1 FROM temp._lithograph_interrupt_guard LIMIT 1)",
            [],
            |row| row.get::<_, bool>(0),
        )
        .map_err(map_prepare_error)?;
    if !empty {
        return Err(incompatible_guard());
    }
    Ok(())
}

fn incompatible_guard() -> LithographError {
    LithographError::storage("query interrupt cleanup requires a compatible empty TEMP guard table")
}

fn map_prepare_error(error: SqliteError) -> LithographError {
    let error = map_sqlite_error(error, "failed to prepare query interrupt cleanup");
    if error.sqlite_code == ffi::SQLITE_INTERRUPT {
        execution::map_query_error(query::QueryError::interrupted())
    } else {
        error
    }
}

/// Returns true only when a real host interrupt was observed and the complete
/// SQLite transaction has actually ended. It never clears SQLite's flags.
pub(super) fn recover(connection: &Connection) -> LithographResult<bool> {
    // SAFETY: callers borrow this live connection for the current callback.
    let db = unsafe { connection.handle() };
    if !host_is_interrupted(db) {
        return Ok(false);
    }
    if !connection.is_autocommit() {
        let mut blob = ptr::null_mut();
        // SAFETY: all names are static NUL-terminated strings; the TEMP table
        // was created before execution. Its absent rowid zero cannot expose
        // host data. With the outer VM still active, SQLite observes the real
        // interrupt before the writable blob VM can access a row.
        let code = unsafe {
            ffi::sqlite3_blob_open(
                db,
                c"temp".as_ptr(),
                TABLE.as_ptr(),
                c"payload".as_ptr(),
                0,
                1,
                &mut blob,
            )
        };
        let close_code = if !blob.is_null() {
            // SAFETY: a non-NULL handle returned by SQLite is owned here.
            unsafe { ffi::sqlite3_blob_close(blob) }
        } else {
            ffi::SQLITE_OK
        };
        if code != ffi::SQLITE_INTERRUPT
            || close_code != ffi::SQLITE_OK
            || !connection.is_autocommit()
        {
            quarantine_connection(connection);
            return Err(LithographError::internal(format!(
                "interrupted execution cleanup failed (SQLite blob recovery code {code}, close code {close_code}, transaction still active: {})",
                !connection.is_autocommit(),
            )));
        }
    }
    clear_explicit_transaction_state(db);
    Ok(true)
}
