use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, prepare,
};
use lithograph_core::storage::{
    HashId, STORAGE_FORMAT, branch_head, create_storage_schema, initialize_connection_state,
    initialize_root, load_snapshot_state,
};
use rusqlite::Connection;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                 id INTEGER PRIMARY KEY CHECK(id=1),\
                 magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL\
             );",
        )
        .expect("metadata");
    connection
        .execute(
            "INSERT INTO main._lithograph_meta VALUES \
             (1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000018', ?1)",
            [STORAGE_FORMAT],
        )
        .expect("metadata marker");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    initialize_connection_state(&connection).expect("connection state");
    connection
}

fn execute_params(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    text_options: &str,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let options = ExecutionOptions::parse_text(text_options).expect("valid access options");
    let prepared = prepare(connection, query, params, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 64)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(connection)?));
        }
    }
}

fn execute(
    connection: &Connection,
    query: &str,
    options: &str,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    execute_params(connection, query, BTreeMap::new(), options)
}

fn call(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    execute(connection, query, "{}")
        .unwrap_or_else(|error| panic!("{query}: {error}"))
        .0
}

fn descriptor(commit: HashId) -> String {
    format!("commit/{}", commit.to_hex())
}

fn string(value: &Value) -> &str {
    let Value::String(value) = value else {
        panic!("expected string, got {value:?}");
    };
    value
}

fn integer(value: &Value) -> i64 {
    let Value::Integer(value) = value else {
        panic!("expected integer, got {value:?}");
    };
    *value
}

fn commit_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count")
}

fn branch(connection: &Connection, name: &str, from: HashId) {
    call(
        connection,
        &format!(
            "CALL lithograph.branch.create('{name}', '{}') YIELD name RETURN name",
            descriptor(from)
        ),
    );
}

fn assert_denied_unchanged(connection: &Connection, query: &str, options: &str, branch: &str) {
    let head = branch_head(connection, branch).expect("initial head");
    let state = load_snapshot_state(connection, head).expect("initial canonical state");
    let commits = commit_count(connection);
    let error = execute(connection, query, options).expect_err("access must be denied");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied, "{query}: {error}");
    assert_eq!(
        branch_head(connection, branch).expect("preserved head"),
        head
    );
    assert_eq!(commit_count(connection), commits);
    assert_eq!(
        load_snapshot_state(connection, head).expect("preserved canonical state"),
        state
    );
}

#[path = "phase18_version_access/canonical.rs"]
mod canonical;
#[path = "phase18_version_access/effects.rs"]
mod effects;
#[path = "phase18_version_access/history.rs"]
mod history;
#[path = "phase18_version_access/merge.rs"]
mod merge;
#[path = "phase18_version_access/patch.rs"]
mod patch;
#[path = "phase18_version_access/references.rs"]
mod references;
#[path = "phase18_version_access/schema_canonical.rs"]
mod schema_canonical;
