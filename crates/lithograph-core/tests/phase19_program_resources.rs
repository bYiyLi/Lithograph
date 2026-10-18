use std::cell::Cell;
use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, prepare};
use lithograph_core::storage::{branch_head, create_storage_schema, initialize_root};
use rusqlite::Connection;

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage");
    initialize_root(&connection).expect("root");
    connection
}

fn make_cursor(connection: &Connection, query: &str, limit: u64) -> QueryCursor {
    let options = ExecutionOptions::parse_text(&format!(r#"{{"memoryLimitBytes":{limit}}}"#))
        .expect("options");
    QueryCursor::new(prepare(connection, query, BTreeMap::new(), options).expect(query))
}

#[test]
fn nested_program_expression_cancels_inside_the_first_result() {
    let connection = fixture();
    for query in [
        "UNWIND [1] AS seed RETURN reduce(s=0, x IN range(1,1000) | s + reduce(t=0,y IN range(1,1000)|t+y)) AS result",
        "UNWIND [1] AS seed RETURN count(*), reduce(s=0, x IN range(1,1000) | s + reduce(t=0,y IN range(1,1000)|t+y)) AS result",
        "RETURN [x IN range(1,1000) | COUNT { UNWIND range(1,1000) AS y RETURN y }] AS result",
    ] {
        let mut cursor = make_cursor(&connection, query, 256 * 1024 * 1024);
        let resources = cursor.resources().clone();
        let checks = Cell::new(0_u64);
        let error = cursor
            .next_batch_with_interrupt(&connection, 1, &|| {
                checks.set(checks.get() + 1);
                checks.get() >= 25
            })
            .expect_err("cancel inside expression");
        assert_eq!(error.kind, QueryErrorKind::Interrupted, "{query}");
        assert!(checks.get() < 40, "bounded cancellation: {query}");
        drop(cursor);
        assert_eq!(resources.used(), 0, "failed program released reservations");
    }
}

#[test]
fn streaming_cross_product_reuses_memory_and_early_drop_releases_lists() {
    let connection = fixture();
    let mut cursor = make_cursor(
        &connection,
        "UNWIND range(1,100) AS x UNWIND range(1,100) AS y RETURN x * 100 + y AS value",
        4 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    let mut count = 0;
    loop {
        let batch = cursor
            .next_batch(&connection, 1)
            .expect("bounded streaming");
        count += batch.rows.len();
        if batch.done {
            break;
        }
    }
    assert_eq!(count, 10_000);
    assert!(resources.peak() < 4 * 1024 * 1024);
    drop(cursor);
    assert_eq!(resources.used(), 0);

    let mut cursor = make_cursor(
        &connection,
        "UNWIND range(1,1000) AS x RETURN x",
        4 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    assert_eq!(
        cursor.next_batch(&connection, 1).expect("first row").rows,
        vec![vec![Value::Integer(1)]]
    );
    assert!(resources.used() > 0, "UNWIND retains its remaining input");
    drop(cursor);
    assert_eq!(resources.used(), 0, "early close releases pending input");
}

#[test]
fn program_aggregate_rejects_expanding_work_before_materializing_all_rows() {
    let connection = fixture();
    let mut cursor = make_cursor(
        &connection,
        "UNWIND range(1,1000) AS x UNWIND range(1,1000) AS y RETURN count(*) AS total",
        4 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("fallback expansion exceeds budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(resources.peak() <= 4 * 1024 * 1024);
    drop(cursor);
    assert_eq!(resources.used(), 0);
}

#[test]
fn discarded_stream_rows_release_temporary_expression_claims() {
    let connection = fixture();
    for query in [
        "UNWIND range(1,100) AS x UNWIND range(1,1000) AS y WITH x,y WHERE x < 0 RETURN x",
        "UNWIND range(1,100) AS x UNWIND range(1,1000) AS y RETURN x * 1000 + y AS value SKIP 100000",
    ] {
        let mut cursor = make_cursor(&connection, query, 4 * 1024 * 1024);
        let resources = cursor.resources().clone();
        let batch = cursor
            .next_batch(&connection, 1)
            .expect("discarded rows return their temporary memory");
        assert!(batch.rows.is_empty(), "{query}");
        assert!(batch.done, "{query}");
        assert!(resources.peak() <= 4 * 1024 * 1024);
        drop(batch);
        drop(cursor);
        assert_eq!(resources.used(), 0);
    }
}

fn commit_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count")
}

fn assert_budget_failure_is_atomic(query: &str, limit: u64) {
    let connection = fixture();
    let before = branch_head(&connection, "main").expect("initial branch");
    let before_commits = commit_count(&connection);
    let mut cursor = make_cursor(&connection, query, limit);
    let resources = cursor.resources().clone();
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("correlated value copies exceed the execution budget");
    assert_eq!(error.kind, QueryErrorKind::Resource, "{query}: {error}");
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(resources.peak() <= limit, "{query}");
    assert_eq!(
        resources.used(),
        0,
        "failed live cursor releases its leases"
    );
    assert_eq!(branch_head(&connection, "main").unwrap(), before, "{query}");
    assert_eq!(commit_count(&connection), before_commits, "{query}");
    assert!(connection.is_autocommit(), "{query}");
    let mut count = make_cursor(&connection, "MATCH (n) RETURN count(n)", 4 * 1024 * 1024);
    assert_eq!(
        count
            .next_batch(&connection, 1)
            .expect("recovered connection")
            .rows,
        vec![vec![Value::Integer(0)]],
        "{query} leaves no staged nodes"
    );
}

#[test]
fn mutation_call_correlation_reserves_each_outer_copy_and_rolls_back_on_rejection() {
    for query in [
        "CREATE (:Pending) WITH range(1,20000) AS big CALL () { UNWIND range(1,200) AS n RETURN n } RETURN count(*)",
        "CREATE (:Pending) WITH range(1,20000) AS big CALL (big) { UNWIND range(1,200) AS n RETURN n } RETURN count(*)",
        "CREATE (:Pending) WITH range(1,20000) AS big CALL { WITH big UNWIND range(1,200) AS n RETURN n } RETURN count(*)",
    ] {
        assert_budget_failure_is_atomic(query, 16 * 1024 * 1024);
    }
}

#[test]
fn mutation_call_correlation_keeps_rows_and_values_when_budget_is_sufficient() {
    for call in [
        "CALL () { UNWIND range(1,4) AS n RETURN n }",
        "CALL (big) { UNWIND range(1,4) AS n RETURN n }",
        "CALL { WITH big UNWIND range(1,4) AS n RETURN n }",
    ] {
        let connection = fixture();
        let query = format!(
            "CREATE (:Committed) WITH range(1,128) AS big {call} RETURN count(*) AS total, max(size(big)) AS width"
        );
        let before_commits = commit_count(&connection);
        let mut cursor = make_cursor(&connection, &query, 16 * 1024 * 1024);
        let resources = cursor.resources().clone();
        let batch = cursor.next_batch(&connection, 1).expect("correlated CALL");
        assert_eq!(
            batch.rows,
            vec![vec![Value::Integer(4), Value::Integer(128)]]
        );
        drop(batch);
        cursor
            .complete(&connection)
            .expect("publish completed query");
        drop(cursor);
        assert_eq!(resources.used(), 0);
        assert_eq!(commit_count(&connection), before_commits + 1);
        assert!(connection.is_autocommit());
    }
}

#[test]
fn foreach_local_copy_is_budgeted_and_failure_keeps_the_branch_unchanged() {
    assert_budget_failure_is_atomic(
        "CREATE (:Pending) WITH range(1,20000) AS big FOREACH (i IN [1] | CREATE (:Inner {items:big})) RETURN size(big)",
        4 * 1024 * 1024,
    );
}

#[test]
fn foreach_local_copy_preserves_outer_values_and_iteration_effects() {
    let connection = fixture();
    let before_commits = commit_count(&connection);
    let mut cursor = make_cursor(
        &connection,
        "CREATE (:Committed) WITH range(1,128) AS big FOREACH (i IN [1,2,3] | CREATE (:Inner {n:i,width:size(big)})) RETURN size(big)",
        16 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    let batch = cursor
        .next_batch(&connection, 1)
        .expect("FOREACH copies fit");
    assert_eq!(batch.rows, vec![vec![Value::Integer(128)]]);
    drop(batch);
    cursor
        .complete(&connection)
        .expect("publish FOREACH effects");
    drop(cursor);
    assert_eq!(resources.used(), 0);
    assert_eq!(commit_count(&connection), before_commits + 1);
    let mut count = make_cursor(
        &connection,
        "MATCH (n:Inner) RETURN count(*),sum(n.n),min(n.width)",
        4 * 1024 * 1024,
    );
    assert_eq!(
        count
            .next_batch(&connection, 1)
            .expect("FOREACH effects")
            .rows,
        vec![vec![
            Value::Integer(3),
            Value::Integer(6),
            Value::Integer(128)
        ]]
    );
}
