use std::cell::Cell;
use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, prepare};
use lithograph_core::storage::{branch_head, create_storage_schema, initialize_root};
use rusqlite::Connection;

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn cursor(connection: &Connection, query: &str, bytes: u64) -> QueryCursor {
    let options =
        ExecutionOptions::parse(&serde_json::json!({"memoryLimitBytes": bytes})).expect("options");
    QueryCursor::new(prepare(connection, query, BTreeMap::new(), options).expect(query))
}

fn node_count(connection: &Connection) -> i64 {
    let mut cursor = cursor(connection, "MATCH (n) RETURN count(n)", 4 * 1024 * 1024);
    let row = cursor.next_batch(connection, 1).expect("count");
    let Value::Integer(count) = row.rows[0][0] else {
        panic!("integer count")
    };
    count
}

#[test]
fn cancellation_inside_mutation_property_rolls_back_without_leaking_budget() {
    let connection = fixture();
    let before = branch_head(&connection, "main").expect("initial head");
    let mut cursor = cursor(
        &connection,
        "CREATE (n:Pending) SET n.total = reduce(s=0,x IN range(1,1000)|s+reduce(t=0,y IN range(1,1000)|t+y)) RETURN n.total",
        256 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    let checks = Cell::new(0_u64);
    let error = cursor
        .next_batch_with_interrupt(&connection, 1, &|| {
            checks.set(checks.get() + 1);
            checks.get() >= 30
        })
        .expect_err("interrupt inside SET expression");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(
        resources.used(),
        0,
        "failure releases leases while cursor remains alive"
    );
    assert_eq!(branch_head(&connection, "main").unwrap(), before);
    assert_eq!(node_count(&connection), 0);
    assert!(connection.is_autocommit());
}

#[test]
fn mutation_program_memory_failure_leaves_no_commit_or_partial_graph() {
    let connection = fixture();
    let before = branch_head(&connection, "main").unwrap();
    let mut cursor = cursor(
        &connection,
        "UNWIND [1] AS i CREATE (n:Pending {items:range(1,500000)}) RETURN n",
        16 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("property list exceeds budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(resources.peak() <= 16 * 1024 * 1024);
    assert_eq!(resources.used(), 0);
    assert_eq!(branch_head(&connection, "main").unwrap(), before);
    assert_eq!(node_count(&connection), 0);
}

#[test]
fn lazy_write_projection_budget_failure_rolls_back_prepared_commit() {
    let connection = fixture();
    let before = branch_head(&connection, "main").unwrap();
    let mut cursor = cursor(
        &connection,
        "CREATE (:Pending) RETURN range(1,500000) AS items",
        16 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("lazy projection exceeds budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), 0);
    assert_eq!(branch_head(&connection, "main").unwrap(), before);
    assert_eq!(node_count(&connection), 0);
}

#[test]
fn resource_failure_in_transaction_batch_is_terminal_and_keeps_durable_batches() {
    let connection = fixture();
    let before = branch_head(&connection, "main").unwrap();
    let mut cursor = cursor(
        &connection,
        "UNWIND [1,2] AS i CALL (i) { CREATE (:Batch {items: CASE WHEN i = 1 THEN [1] ELSE range(1,500000) END}) } IN TRANSACTIONS OF 1 ROW ON ERROR CONTINUE FINISH",
        16 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("execution quota cannot be swallowed by ON ERROR CONTINUE");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), 0);
    assert_ne!(branch_head(&connection, "main").unwrap(), before);
    assert_eq!(
        node_count(&connection),
        1,
        "only completed first batch is durable"
    );
    assert!(connection.is_autocommit());
}

#[test]
fn early_write_cancel_releases_output_and_rolls_back_unpublished_write() {
    let connection = fixture();
    let before = branch_head(&connection, "main").unwrap();
    let mut cursor = cursor(
        &connection,
        "CREATE (:Pending) RETURN range(1,100) AS items",
        4 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    assert_eq!(cursor.next_batch(&connection, 1).unwrap().rows.len(), 1);
    assert!(resources.used() > 0);
    cursor
        .cancel(&connection)
        .expect("cancel before completion publishes write");
    assert_eq!(resources.used(), 0);
    assert_eq!(branch_head(&connection, "main").unwrap(), before);
    assert_eq!(node_count(&connection), 0);
}

fn commit_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count")
}

#[test]
fn concurrent_transaction_batches_share_the_parent_execution_budget() {
    const QUERY: &str = "UNWIND [1,2] AS i CALL (i) { CREATE (:SharedQuota {i:i,total:size(range(1,20000))}) } IN 2 CONCURRENT TRANSACTIONS OF 1 ROWS DISJOINT BY (i) FINISH";
    let control_connection = fixture();
    let before_control_commits = commit_count(&control_connection);
    let mut control = cursor(&control_connection, QUERY, 16 * 1024 * 1024);
    let result = control
        .next_batch(&control_connection, 1)
        .expect("normal concurrent batches have enough shared memory");
    assert!(result.done);
    assert!(result.rows.is_empty());
    drop(result);
    control
        .complete(&control_connection)
        .expect("complete normal control");
    assert_eq!(node_count(&control_connection), 2);
    assert_eq!(
        commit_count(&control_connection),
        before_control_commits + 2
    );

    let connection = fixture();
    let before = branch_head(&connection, "main").unwrap();
    let before_commits = commit_count(&connection);
    let mut cursor = cursor(&connection, QUERY, 16 * 1024 * 1024);
    let resources = cursor.resources().clone();
    let limit = resources.limit().expect("explicit quota");
    let sentinel = resources
        .reserve(limit - resources.used() - 256 * 1024)
        .expect("parent reservation leaving bounded headroom");
    let sentinel_bytes = sentinel.bytes();
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("transaction batch must see the parent's existing reservation");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_eq!(branch_head(&connection, "main").unwrap(), before);
    assert_eq!(commit_count(&connection), before_commits);
    assert_eq!(
        resources.used(),
        sentinel_bytes,
        "only caller-owned sentinel remains while failed cursor stays alive"
    );
    drop(sentinel);
    assert_eq!(resources.used(), 0);
    assert_eq!(node_count(&connection), 0);
    assert!(connection.is_autocommit());
}

#[test]
fn later_transaction_batch_interrupt_keeps_only_the_first_durable_commit() {
    let connection = fixture();
    let before_commits = commit_count(&connection);
    let mut cursor = cursor(
        &connection,
        "UNWIND [1,2] AS i CALL (i) { CREATE (:CancelledBatch {i:i,total:CASE WHEN i = 1 THEN 1 ELSE reduce(s=0,x IN range(1,5000)|s+reduce(t=0,y IN range(1,100)|t+y)) END}) RETURN i AS value } IN TRANSACTIONS OF 1 ROWS ON ERROR CONTINUE RETURN value",
        256 * 1024 * 1024,
    );
    let resources = cursor.resources().clone();
    let observed_durable_head = Cell::new(None);
    let signaled_inside_large_expression = Cell::new(false);
    // A mutating subquery can defer output until the final Snapshot. Observe
    // the actual autocommit boundary, rather than assuming a returned row is
    // the first batch boundary.
    let error = cursor
        .next_batch_with_interrupt(&connection, 1, &|| {
            if observed_durable_head.get().is_none()
                && connection.is_autocommit()
                && commit_count(&connection) == before_commits + 1
            {
                observed_durable_head.set(Some(
                    branch_head(&connection, "main").expect("durable first batch head"),
                ));
            }
            if observed_durable_head.get().is_some() && resources.used() >= 512 * 1024 {
                signaled_inside_large_expression.set(true);
                true
            } else {
                false
            }
        })
        .expect_err("interrupt second batch after its long expression starts");
    assert!(signaled_inside_large_expression.get());
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(
        resources.used(),
        0,
        "failed cursor releases all its leases without being dropped"
    );
    assert_eq!(
        branch_head(&connection, "main").unwrap(),
        observed_durable_head
            .get()
            .expect("first batch was durably committed before cancellation")
    );
    assert_eq!(commit_count(&connection), before_commits + 1);
    assert_eq!(node_count(&connection), 1);
    assert!(connection.is_autocommit());
}
