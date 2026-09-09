//! SQLite loadable-extension boundary for Lithograph.
//!
//! Phase 00 exposes only the real SQLite loading entry point. It deliberately
//! registers no Lithograph product API; Phase 01 owns those public surfaces.

use std::ffi::{c_char, c_int};

use rusqlite::{Connection, Result, ffi};

/// Entry point used by stock SQLite when loading the Lithograph shared library.
///
/// Phase 00 uses this no-op initializer solely to validate the actual
/// loadable-extension ABI and test harness without mocking product behavior.
///
/// # Safety
///
/// SQLite calls this function with pointers governed by the loadable-extension
/// ABI. The pointers must originate from the SQLite host that is loading this
/// shared library.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn sqlite3_lithograph_init(
    db: *mut ffi::sqlite3,
    pz_err_msg: *mut *mut c_char,
    p_api: *mut ffi::sqlite3_api_routines,
) -> c_int {
    unsafe { Connection::extension_init2(db, pz_err_msg, p_api, extension_init) }
}

fn extension_init(_db: Connection) -> Result<bool> {
    Ok(false)
}
