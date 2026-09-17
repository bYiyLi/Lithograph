//! Narrow SQLite FTS5 ABI boundary used by Lithograph full-text search.
//!
//! `lithograph-core` intentionally forbids unsafe code. FTS5 custom tokenizer
//! delegation is available only through the native `fts5_api`, so the required
//! FFI is isolated in this crate.

#![allow(
    unsafe_code,
    reason = "FTS5 tokenizer registration and callback delegation require the SQLite native ABI"
)]
#![cfg_attr(
    not(test),
    deny(clippy::expect_used, clippy::panic, clippy::unwrap_used)
)]

use std::collections::HashMap;
use std::ffi::{CStr, CString, c_char, c_int, c_void};
use std::fmt;
use std::marker::PhantomData;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::ptr;
use std::sync::{Mutex, OnceLock};

use rusqlite::Connection;
use rusqlite::ffi;
use rusqlite::types::ToSqlOutput;

const INTERNAL_TOKENIZER: &str = "lithograph_dual_v1";
const FTS5_API_PTR: &CStr = c"fts5_api_ptr";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Fts5Error {
    pub sqlite_code: i32,
    pub message: String,
}

impl Fts5Error {
    fn new(sqlite_code: i32, message: impl Into<String>) -> Self {
        Self {
            sqlite_code,
            message: message.into(),
        }
    }
}

impl fmt::Display for Fts5Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.message)
    }
}

impl std::error::Error for Fts5Error {}

impl From<rusqlite::Error> for Fts5Error {
    fn from(error: rusqlite::Error) -> Self {
        let sqlite_code = match &error {
            rusqlite::Error::SqliteFailure(code, _) => code.extended_code & 0xff,
            _ => ffi::SQLITE_ERROR,
        };
        Self::new(sqlite_code, error.to_string())
    }
}

/// Installs Lithograph's private dual-tokenizer adapter on this SQLite
/// connection. A host registration using the reserved name is never replaced.
pub fn ensure_dual_tokenizer(connection: &Connection) -> Result<(), Fts5Error> {
    let api = fts5_api(connection)?;
    let name = CString::new(INTERNAL_TOKENIZER)
        .map_err(|_| Fts5Error::new(ffi::SQLITE_ERROR, "invalid internal tokenizer name"))?;
    let mut user_data = ptr::null_mut();
    let mut tokenizer = empty_tokenizer();
    // SAFETY: `api` is the live connection-owned FTS5 API pointer.
    let find = unsafe { (*api).xFindTokenizer }
        .ok_or_else(|| Fts5Error::new(ffi::SQLITE_ERROR, "FTS5 tokenizer lookup is unavailable"))?;
    // SAFETY: `api` is the live connection-owned FTS5 API and output pointers
    // refer to initialized local variables for the duration of the call.
    let found = unsafe { find(api, name.as_ptr(), &mut user_data, &mut tokenizer) };
    if found == ffi::SQLITE_OK {
        if tokenizer_is_ours(&tokenizer) && user_data == api.cast::<c_void>() {
            return Ok(());
        }
        return Err(Fts5Error::new(
            ffi::SQLITE_ERROR,
            "the reserved Lithograph FTS5 tokenizer name is already registered by the host",
        ));
    }
    if found != ffi::SQLITE_ERROR {
        return Err(Fts5Error::new(
            found,
            "failed to inspect the SQLite FTS5 tokenizer registry",
        ));
    }

    // SAFETY: `api` remains valid for the lifetime of this SQLite connection.
    let create = unsafe { (*api).xCreateTokenizer }.ok_or_else(|| {
        Fts5Error::new(
            ffi::SQLITE_ERROR,
            "FTS5 tokenizer registration is unavailable",
        )
    })?;
    let mut adapter = ffi::fts5_tokenizer {
        xCreate: Some(dual_create),
        xDelete: Some(dual_delete),
        xTokenize: Some(dual_tokenize),
    };
    // SAFETY: FTS5 retains this connection-owned API pointer only as opaque
    // user data. It is valid for exactly the tokenizer registration lifetime.
    let code = unsafe { create(api, name.as_ptr(), api.cast::<c_void>(), &mut adapter, None) };
    if code == ffi::SQLITE_OK {
        Ok(())
    } else {
        Err(Fts5Error::new(
            code,
            "failed to register the Lithograph FTS5 tokenizer adapter",
        ))
    }
}

/// Returns an FTS5 tokenizer specification for a connection-local adapter
/// instance. The index tokenizer is stable for the table lifetime; the query
/// tokenizer is the initial scoped value.
pub fn dual_tokenizer_spec(index_spec: &str, query_spec: &str, handle: &str) -> String {
    format!(
        "{INTERNAL_TOKENIZER} {} {} {}",
        quote_fts5_argument(index_spec),
        quote_fts5_argument(query_spec),
        quote_fts5_argument(handle)
    )
}

/// Installs a query tokenizer for one adapter instance until the returned guard
/// is dropped. Nested guards are LIFO-safe: each guard restores the child that
/// was active when it was created.
pub fn override_query_tokenizer<'a>(
    connection: &'a Connection,
    handle: &str,
    query_spec: &str,
) -> Result<QueryTokenizerOverride<'a>, Fts5Error> {
    let api = fts5_api(connection)?;
    let api_key = api as usize;
    let pointer = instances()
        .lock()
        .map_err(|_| {
            Fts5Error::new(
                ffi::SQLITE_ERROR,
                "FTS5 tokenizer instance lock is poisoned",
            )
        })?
        .get(&(api_key, handle.to_owned()))
        .copied()
        .ok_or_else(|| {
            Fts5Error::new(
                ffi::SQLITE_ERROR,
                "FTS5 tokenizer adapter instance is unavailable",
            )
        })?;
    let arguments = parse_specification(query_spec)
        .map_err(|code| Fts5Error::new(code, "invalid FTS5 tokenizer specification"))?;
    // SAFETY: the instance registry is connection-local and xDelete removes
    // the entry before destroying the allocation. Core keeps the owning FTS5
    // table alive for the entire guard lifetime.
    let replacement = unsafe { create_child(api, &arguments) }
        .map_err(|code| Fts5Error::new(code, "failed to construct FTS5 query tokenizer"))?;
    let tokenizer_ptr = pointer as *mut DualTokenizer;
    // SAFETY: SQLite serializes use of one connection. The owning table remains
    // alive while Core creates this scoped guard.
    let tokenizer = unsafe { &mut *tokenizer_ptr };
    let previous = std::mem::replace(&mut tokenizer.query, replacement);
    Ok(QueryTokenizerOverride {
        tokenizer: tokenizer_ptr,
        previous: Some(previous),
        _connection: PhantomData,
    })
}

pub struct QueryTokenizerOverride<'a> {
    tokenizer: *mut DualTokenizer,
    previous: Option<ChildTokenizer>,
    _connection: PhantomData<&'a Connection>,
}

impl Drop for QueryTokenizerOverride<'_> {
    fn drop(&mut self) {
        let Some(previous) = self.previous.take() else {
            return;
        };
        // SAFETY: the owning FTS5 table remains alive for the guard lifetime;
        // guards created by Core are dropped in LIFO scope order.
        let tokenizer = unsafe { &mut *self.tokenizer };
        let replacement = std::mem::replace(&mut tokenizer.query, previous);
        // SAFETY: the replacement child is no longer reachable from adapter.
        unsafe { delete_child(replacement) };
    }
}

fn tokenizer_is_ours(tokenizer: &ffi::fts5_tokenizer) -> bool {
    tokenizer.xCreate.map(|callback| callback as usize) == Some(dual_create as *const () as usize)
        && tokenizer.xDelete.map(|callback| callback as usize)
            == Some(dual_delete as *const () as usize)
        && tokenizer.xTokenize.map(|callback| callback as usize)
            == Some(dual_tokenize as *const () as usize)
}

fn quote_fts5_argument(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn instances() -> &'static Mutex<HashMap<(usize, String), usize>> {
    static INSTANCES: OnceLock<Mutex<HashMap<(usize, String), usize>>> = OnceLock::new();
    INSTANCES.get_or_init(|| Mutex::new(HashMap::new()))
}

struct ChildTokenizer {
    api: ffi::fts5_tokenizer,
    instance: *mut ffi::Fts5Tokenizer,
}

struct DualTokenizer {
    api_key: usize,
    handle: String,
    index: ChildTokenizer,
    query: ChildTokenizer,
}

fn fts5_api(connection: &Connection) -> Result<*mut ffi::fts5_api, Fts5Error> {
    let mut api: *mut ffi::fts5_api = ptr::null_mut();
    let pointer = ToSqlOutput::Pointer((
        (&mut api as *mut *mut ffi::fts5_api).cast::<c_void>(),
        FTS5_API_PTR,
        None,
    ));
    connection.query_row("SELECT fts5(?1)", [pointer], |_| Ok(()))?;
    if api.is_null() {
        Err(Fts5Error::new(
            ffi::SQLITE_ERROR,
            "SQLite did not expose the FTS5 API for this connection",
        ))
    } else {
        Ok(api)
    }
}

fn empty_tokenizer() -> ffi::fts5_tokenizer {
    ffi::fts5_tokenizer {
        xCreate: None,
        xDelete: None,
        xTokenize: None,
    }
}

unsafe extern "C" fn dual_create(
    user_data: *mut c_void,
    args: *mut *const c_char,
    arg_count: c_int,
    output: *mut *mut ffi::Fts5Tokenizer,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        // SAFETY: all pointers and counts come from FTS5 for this registered
        // tokenizer and remain valid for the callback duration.
        unsafe { dual_create_impl(user_data, args, arg_count, output) }
    }))
    .unwrap_or(ffi::SQLITE_ERROR)
}

unsafe fn dual_create_impl(
    user_data: *mut c_void,
    args: *mut *const c_char,
    arg_count: c_int,
    output: *mut *mut ffi::Fts5Tokenizer,
) -> c_int {
    if user_data.is_null() || output.is_null() || arg_count != 3 || args.is_null() {
        return ffi::SQLITE_ERROR;
    }
    let api = user_data.cast::<ffi::fts5_api>();
    // SAFETY: FTS5 supplied exactly three constructor argument pointers.
    let arguments = unsafe { std::slice::from_raw_parts(args, 3) };
    // SAFETY: FTS5 supplied NUL-terminated constructor arguments that remain
    // valid for the duration of this callback.
    let Some(index_spec) = (unsafe { c_string_argument(arguments[0]) }) else {
        return ffi::SQLITE_ERROR;
    };
    // SAFETY: same constructor-argument lifetime contract as above.
    let Some(query_spec) = (unsafe { c_string_argument(arguments[1]) }) else {
        return ffi::SQLITE_ERROR;
    };
    // SAFETY: same constructor-argument lifetime contract as above.
    let Some(handle) = (unsafe { c_string_argument(arguments[2]) }) else {
        return ffi::SQLITE_ERROR;
    };
    if handle.is_empty() {
        return ffi::SQLITE_ERROR;
    }
    let index_args = match parse_specification(&index_spec) {
        Ok(args) => args,
        Err(code) => return code,
    };
    let query_args = match parse_specification(&query_spec) {
        Ok(args) => args,
        Err(code) => return code,
    };
    // SAFETY: `api` is the live connection-owned FTS5 API stored as user data.
    let index = match unsafe { create_child(api, &index_args) } {
        Ok(child) => child,
        Err(code) => return code,
    };
    // SAFETY: same connection/API contract as above.
    let query = match unsafe { create_child(api, &query_args) } {
        Ok(child) => child,
        Err(code) => {
            // SAFETY: `index` has not been transferred elsewhere.
            unsafe { delete_child(index) };
            return code;
        }
    };
    let tokenizer = Box::new(DualTokenizer {
        api_key: api as usize,
        handle: handle.clone(),
        index,
        query,
    });
    let tokenizer_ptr = Box::into_raw(tokenizer);
    let key = (api as usize, handle);
    let inserted = match instances().lock() {
        Ok(mut instances) => match instances.entry(key) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(tokenizer_ptr as usize);
                true
            }
            std::collections::hash_map::Entry::Occupied(_) => false,
        },
        Err(_) => false,
    };
    if !inserted {
        // SAFETY: ownership has not been transferred to FTS5 yet.
        let tokenizer = unsafe { Box::from_raw(tokenizer_ptr) };
        // SAFETY: both children are uniquely owned by this allocation.
        unsafe { delete_child(tokenizer.index) };
        // SAFETY: both children are uniquely owned by this allocation.
        unsafe { delete_child(tokenizer.query) };
        return ffi::SQLITE_ERROR;
    }
    // SAFETY: xDelete casts this opaque pointer back to `DualTokenizer`.
    unsafe { *output = tokenizer_ptr.cast::<ffi::Fts5Tokenizer>() };
    ffi::SQLITE_OK
}

unsafe fn create_child(api: *mut ffi::fts5_api, args: &[String]) -> Result<ChildTokenizer, c_int> {
    let Some(name) = args.first() else {
        return Err(ffi::SQLITE_ERROR);
    };
    if name.eq_ignore_ascii_case(INTERNAL_TOKENIZER) {
        return Err(ffi::SQLITE_ERROR);
    }
    let name = CString::new(name.as_str()).map_err(|_| ffi::SQLITE_ERROR)?;
    let mut user_data = ptr::null_mut();
    let mut tokenizer = empty_tokenizer();
    // SAFETY: `api` is a valid connection-owned FTS5 API pointer.
    let find = unsafe { (*api).xFindTokenizer }.ok_or(ffi::SQLITE_ERROR)?;
    // SAFETY: output pointers are initialized locals valid for the call.
    let code = unsafe { find(api, name.as_ptr(), &mut user_data, &mut tokenizer) };
    if code != ffi::SQLITE_OK {
        return Err(code);
    }
    let create = tokenizer.xCreate.ok_or(ffi::SQLITE_ERROR)?;
    let c_args = args
        .iter()
        .skip(1)
        .map(|value| CString::new(value.as_str()).map_err(|_| ffi::SQLITE_ERROR))
        .collect::<Result<Vec<_>, _>>()?;
    let mut raw_args = c_args
        .iter()
        .map(|value| value.as_ptr())
        .collect::<Vec<_>>();
    let mut instance = ptr::null_mut();
    let count = c_int::try_from(raw_args.len()).map_err(|_| ffi::SQLITE_TOOBIG)?;
    // SAFETY: C strings live through the constructor call; output is a valid
    // local pointer; the tokenizer module owns the constructor contract.
    let code = unsafe { create(user_data, raw_args.as_mut_ptr(), count, &mut instance) };
    if code != ffi::SQLITE_OK || instance.is_null() {
        return Err(if code == ffi::SQLITE_OK {
            ffi::SQLITE_ERROR
        } else {
            code
        });
    }
    Ok(ChildTokenizer {
        api: tokenizer,
        instance,
    })
}

unsafe fn delete_child(child: ChildTokenizer) {
    if let Some(delete) = child.api.xDelete {
        // SAFETY: `instance` was returned by the matching xCreate and is owned
        // exactly once by this ChildTokenizer.
        unsafe { delete(child.instance) };
    }
}

unsafe extern "C" fn dual_delete(tokenizer: *mut ffi::Fts5Tokenizer) {
    let _ = catch_unwind(AssertUnwindSafe(|| {
        if tokenizer.is_null() {
            return;
        }
        // SAFETY: pointer was allocated by dual_create for this tokenizer and
        // FTS5 invokes xDelete exactly once.
        let tokenizer = unsafe { Box::from_raw(tokenizer.cast::<DualTokenizer>()) };
        if let Ok(mut instances) = instances().lock() {
            let key = (tokenizer.api_key, tokenizer.handle.clone());
            if instances.get(&key).copied() == Some(tokenizer.as_ref() as *const _ as usize) {
                instances.remove(&key);
            }
        }
        // SAFETY: each child is uniquely owned by this adapter instance.
        unsafe { delete_child(tokenizer.index) };
        // SAFETY: each child is uniquely owned by this adapter instance.
        unsafe { delete_child(tokenizer.query) };
    }));
}

unsafe extern "C" fn dual_tokenize(
    tokenizer: *mut ffi::Fts5Tokenizer,
    context: *mut c_void,
    flags: c_int,
    text: *const c_char,
    text_len: c_int,
    callback: Option<
        unsafe extern "C" fn(*mut c_void, c_int, *const c_char, c_int, c_int, c_int) -> c_int,
    >,
) -> c_int {
    catch_unwind(AssertUnwindSafe(|| {
        if tokenizer.is_null() {
            return ffi::SQLITE_ERROR;
        }
        // Copy the callback and opaque child pointer before entering external
        // tokenizer code. A third-party tokenizer may re-enter the same SQLite
        // connection; nested query guards are then free to replace the scoped
        // query child without mutating through an outstanding Rust reference.
        let (tokenize, instance) = {
            // SAFETY: pointer is alive for the duration of this FTS5 callback.
            let tokenizer = unsafe { &*tokenizer.cast::<DualTokenizer>() };
            let child = if flags & ffi::FTS5_TOKENIZE_QUERY != 0 {
                &tokenizer.query
            } else {
                &tokenizer.index
            };
            (child.api.xTokenize, child.instance)
        };
        let Some(tokenize) = tokenize else {
            return ffi::SQLITE_ERROR;
        };
        // SAFETY: all arguments are forwarded unchanged from FTS5 to the
        // tokenizer instance created by the matching module.
        unsafe { tokenize(instance, context, flags, text, text_len, callback) }
    }))
    .unwrap_or(ffi::SQLITE_ERROR)
}

unsafe fn c_string_argument(value: *const c_char) -> Option<String> {
    if value.is_null() {
        return None;
    }
    // SAFETY: FTS5 constructor arguments are NUL-terminated for the callback.
    unsafe { CStr::from_ptr(value) }
        .to_str()
        .ok()
        .map(str::to_owned)
}

fn parse_specification(specification: &str) -> Result<Vec<String>, c_int> {
    if specification.as_bytes().contains(&0) {
        return Err(ffi::SQLITE_ERROR);
    }
    let bytes = specification.as_bytes();
    let mut offset = 0_usize;
    let mut arguments = Vec::new();
    while offset < bytes.len() {
        while offset < bytes.len() && bytes[offset] == b' ' {
            offset += 1;
        }
        if offset == bytes.len() {
            break;
        }
        let (argument, next) = if bytes[offset] == b'\'' {
            parse_quoted_argument(specification, offset)?
        } else {
            parse_bare_argument(specification, offset)?
        };
        arguments.push(argument);
        offset = next;
        if offset < bytes.len() && bytes[offset] != b' ' {
            return Err(ffi::SQLITE_ERROR);
        }
    }
    if arguments.is_empty() {
        Err(ffi::SQLITE_ERROR)
    } else {
        Ok(arguments)
    }
}

fn parse_quoted_argument(specification: &str, start: usize) -> Result<(String, usize), c_int> {
    let bytes = specification.as_bytes();
    let mut offset = start + 1;
    let mut value = String::new();
    while offset < bytes.len() {
        if bytes[offset] == b'\'' {
            if offset + 1 < bytes.len() && bytes[offset + 1] == b'\'' {
                value.push('\'');
                offset += 2;
                continue;
            }
            return Ok((value, offset + 1));
        }
        let Some(character) = specification[offset..].chars().next() else {
            return Err(ffi::SQLITE_ERROR);
        };
        value.push(character);
        offset += character.len_utf8();
    }
    Err(ffi::SQLITE_ERROR)
}

fn parse_bare_argument(specification: &str, start: usize) -> Result<(String, usize), c_int> {
    let bytes = specification.as_bytes();
    let mut offset = start;
    while offset < bytes.len() && is_bareword_byte(bytes[offset]) {
        offset += 1;
    }
    if offset == start {
        return Err(ffi::SQLITE_ERROR);
    }
    specification
        .get(start..offset)
        .map(|value| (value.to_owned(), offset))
        .ok_or(ffi::SQLITE_ERROR)
}

fn is_bareword_byte(value: u8) -> bool {
    value >= 0x80 || value.is_ascii_alphanumeric() || value == b'_' || value == 0x1a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_matches_fts5_argument_rules() {
        assert_eq!(
            parse_specification("unicode61 tokenchars '-_' 'two words' ''"),
            Ok(vec![
                "unicode61".to_owned(),
                "tokenchars".to_owned(),
                "-_".to_owned(),
                "two words".to_owned(),
                String::new(),
            ])
        );
        assert_eq!(
            parse_specification("tok 'it''s' 中文"),
            Ok(vec!["tok".to_owned(), "it's".to_owned(), "中文".to_owned()])
        );
        assert!(parse_specification("token-with-dash").is_err());
        assert!(parse_specification("tok\targ").is_err());
        assert!(parse_specification("'unterminated").is_err());
        assert!(parse_specification("   ").is_err());
    }

    #[test]
    fn dual_spec_quotes_each_nested_specification() {
        assert_eq!(
            dual_tokenizer_spec("unicode61 tokenchars '-_'", "porter unicode61", "cache one"),
            "lithograph_dual_v1 'unicode61 tokenchars ''-_''' 'porter unicode61' 'cache one'"
        );
    }

    #[test]
    fn host_registration_with_reserved_name_is_not_overwritten() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        let api = fts5_api(&connection).expect("FTS5 API");
        let unicode_name = CString::new("unicode61").expect("unicode tokenizer name");
        let internal_name = CString::new(INTERNAL_TOKENIZER).expect("internal tokenizer name");
        let mut user_data = ptr::null_mut();
        let mut tokenizer = empty_tokenizer();
        // SAFETY: `api` is the live API for this test connection.
        let find = unsafe { (*api).xFindTokenizer }.expect("FTS5 tokenizer lookup");
        // SAFETY: all pointers refer to live local outputs or immutable C strings.
        let find_code = unsafe { find(api, unicode_name.as_ptr(), &mut user_data, &mut tokenizer) };
        assert_eq!(find_code, ffi::SQLITE_OK);
        // SAFETY: `api` is the live API for this test connection.
        let create = unsafe { (*api).xCreateTokenizer }.expect("FTS5 tokenizer registration");
        // SAFETY: SQLite copies the tokenizer module struct for this connection;
        // built-in tokenizer user data remains connection-owned.
        let create_code =
            unsafe { create(api, internal_name.as_ptr(), user_data, &mut tokenizer, None) };
        assert_eq!(create_code, ffi::SQLITE_OK);
        let error = ensure_dual_tokenizer(&connection)
            .expect_err("Lithograph must not replace a host-owned reserved name");
        assert!(error.message.contains("already registered by the host"));
    }

    #[test]
    fn nested_query_overrides_restore_in_lifo_order() {
        let connection = Connection::open_in_memory().expect("open SQLite");
        ensure_dual_tokenizer(&connection).expect("register dual tokenizer");
        let specification = dual_tokenizer_spec("unicode61", "unicode61", "scoped_fts");
        let sql = format!(
            "CREATE VIRTUAL TABLE temp.scoped_fts USING fts5(value, tokenize='{}')",
            specification.replace('\'', "''")
        );
        connection
            .execute_batch(&sql)
            .expect("create delegated table");
        connection
            .execute("INSERT INTO temp.scoped_fts(value) VALUES(?1)", ["run"])
            .expect("insert document");

        let plain = || -> i64 {
            connection
                .query_row(
                    "SELECT count(*) FROM temp.scoped_fts WHERE scoped_fts MATCH ?1",
                    ["running"],
                    |row| row.get(0),
                )
                .expect("query delegated table")
        };
        assert_eq!(plain(), 0);
        {
            let _porter = override_query_tokenizer(&connection, "scoped_fts", "porter unicode61")
                .expect("porter override");
            assert_eq!(plain(), 1);
            {
                let _plain = override_query_tokenizer(&connection, "scoped_fts", "unicode61")
                    .expect("nested plain override");
                assert_eq!(plain(), 0);
            }
            assert_eq!(plain(), 1);
        }
        assert_eq!(plain(), 0);
    }
}
