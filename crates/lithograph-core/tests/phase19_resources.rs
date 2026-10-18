use std::cell::Cell;
use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryErrorKind, QueryResources, prepare,
    prepare_with_resources_and_interrupt,
};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;

fn database() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn options(bytes: u64) -> ExecutionOptions {
    ExecutionOptions::parse(&serde_json::json!({ "memoryLimitBytes": bytes }))
        .expect("memory limit")
}

#[test]
fn range_budget_refuses_before_large_allocation_and_allows_larger_budget() {
    let connection = database();
    let source = "RETURN size(range(1,500000)) AS count";
    let prepared = prepare(
        &connection,
        source,
        BTreeMap::new(),
        options(16 * 1024 * 1024),
    )
    .expect("prepare small budget");
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("range exceeds budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(resources.peak() <= 16 * 1024 * 1024);
    cursor.cancel(&connection).expect("failure cleanup");
    drop(cursor);
    assert_eq!(resources.used(), 0);

    let prepared = prepare(
        &connection,
        source,
        BTreeMap::new(),
        options(256 * 1024 * 1024),
    )
    .expect("prepare sufficient budget");
    let mut cursor = QueryCursor::new(prepared);
    let batch = cursor
        .next_batch(&connection, 1)
        .expect("execute sufficient budget");
    assert_eq!(batch.rows, vec![vec![Value::Integer(500000)]]);
}

#[test]
fn nested_reduce_observes_cancellation_inside_expression() {
    let connection = database();
    let prepared = prepare(
        &connection,
        "RETURN reduce(total=0, x IN range(1,2000) | total + reduce(inner=0, y IN range(1,100) | inner+y)) AS value",
        BTreeMap::new(), options(256 * 1024 * 1024),
    ).expect("prepare nested reduce");
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let checks = Cell::new(0);
    let error = cursor
        .next_batch_with_interrupt(&connection, 1, &|| {
            let next = checks.get() + 1;
            checks.set(next);
            next >= 64
        })
        .expect_err("cancel long expression");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(
        checks.get(),
        64,
        "no remainder of expression after cancellation"
    );
    cursor.cancel(&connection).expect("cancel cleanup");
    drop(cursor);
    assert_eq!(resources.used(), 0);
    assert_eq!(
        connection
            .query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
}

#[test]
fn streamed_rows_reuse_budget_and_cursor_drop_releases_retained_input() {
    let connection = database();
    let prepared = prepare(
        &connection,
        "UNWIND range(1,10000) AS i RETURN i",
        BTreeMap::new(),
        options(16 * 1024 * 1024),
    )
    .expect("prepare stream");
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut count = 0;
    let mut first_used = None;
    loop {
        let batch = cursor.next_batch(&connection, 1).expect("next row");
        count += batch.rows.len();
        if count == 1 {
            first_used = Some(resources.used());
        }
        if count == 5000 {
            assert!(
                resources.used() <= first_used.unwrap() + 64 * 1024,
                "stream budget must reflect live values, not cumulative output"
            );
        }
        if batch.done {
            break;
        }
    }
    assert_eq!(count, 10000);
    drop(cursor);
    assert_eq!(resources.used(), 0);
}

#[test]
fn separate_executions_do_not_share_memory_limits() {
    let connection = database();
    let first = prepare(
        &connection,
        "RETURN range(1,100)",
        BTreeMap::new(),
        options(1024 * 1024),
    )
    .unwrap();
    let second = prepare(
        &connection,
        "RETURN range(1,100)",
        BTreeMap::new(),
        options(1024 * 1024),
    )
    .unwrap();
    let first_resources = first.resources.clone();
    let second_resources = second.resources.clone();
    let mut first = QueryCursor::new(first);
    let mut second = QueryCursor::new(second);
    let first_batch = first.next_batch(&connection, 1).unwrap();
    let first_used = first_resources.used();
    let second_batch = second.next_batch(&connection, 1).unwrap();
    assert_eq!(first_resources.used(), first_used);
    drop(first_batch);
    drop(first);
    assert_eq!(first_resources.used(), 0);
    assert!(second_resources.used() > 0);
    drop(second);
    assert!(second_resources.used() > 0);
    drop(second_batch);
    assert_eq!(second_resources.used(), 0);
}

#[test]
fn schema_arguments_use_budget_and_prepare_cancellation() {
    let connection = database();
    let query = "CALL db.index.semantic.createNodeIndex('idx', ['Person'], 'text', {provider:'p',providerConfig:{items:range(1,500000)},dimensions:4,similarity:'cosine'})";
    let resources = QueryResources::new(Some(16 * 1024 * 1024));
    let error = prepare_with_resources_and_interrupt(
        &connection,
        query,
        BTreeMap::new(),
        options(16 * 1024 * 1024),
        resources.clone(),
        &|| false,
    )
    .expect_err("large schema argument exceeds budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), 0);
    let resources = QueryResources::new(Some(256 * 1024 * 1024));
    let checks = Cell::new(0);
    let error = prepare_with_resources_and_interrupt(
        &connection,
        query,
        BTreeMap::new(),
        options(256 * 1024 * 1024),
        resources.clone(),
        &|| {
            checks.set(checks.get() + 1);
            checks.get() >= 3
        },
    )
    .expect_err("schema argument cancellation");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(resources.used(), 0);
}

fn fixture_query(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 1).unwrap();
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection).unwrap();
            return rows;
        }
    }
}

fn assert_seek_folding_failure(
    connection: &Connection,
    query: &str,
    limit: u64,
    expected: QueryErrorKind,
    is_interrupted: &dyn Fn() -> bool,
) {
    let resources = QueryResources::new(Some(limit));
    let prepared = prepare_with_resources_and_interrupt(
        connection,
        query,
        BTreeMap::new(),
        options(limit),
        resources.clone(),
        is_interrupted,
    );
    let error = match prepared {
        Err(error) => error,
        Ok(prepared) => {
            let mut cursor = QueryCursor::new(prepared);
            let error = cursor
                .next_batch_with_interrupt(connection, 1, is_interrupted)
                .expect_err("index seek folding must use the execution controls");
            assert_eq!(resources.used(), 0, "failed cursor must release memory");
            error
        }
    };
    assert_eq!(error.kind, expected, "query: {query}");
    assert_eq!(resources.used(), 0, "failed execution must release memory");
}

#[test]
fn index_seek_folding_uses_execution_budget_and_interrupt_at_each_entry_boundary() {
    let connection = database();
    for query in [
        "CREATE (:Person {id:1}) FINISH",
        "CREATE CONSTRAINT person_id_type FOR (n:Person) REQUIRE n.id IS :: INTEGER",
        "CREATE RANGE INDEX person_id FOR (n:Person) ON (n.id)",
    ] {
        fixture_query(&connection, query);
    }
    let explain = fixture_query(
        &connection,
        "EXPLAIN MATCH (n:Person) WHERE n.id=1 RETURN n.id",
    );
    assert!(
        explain
            .iter()
            .flatten()
            .any(|value| matches!(value, Value::String(plan) if plan.contains("IndexSeek"))),
        "fixture must enable a real typed range index seek"
    );
    for query in [
        "MATCH (n:Person) WHERE n.id IN range(2,500001) RETURN n.id",
        "MATCH (n:Person) WHERE n.id IN range(2,500001) WITH n RETURN n.id",
    ] {
        assert_seek_folding_failure(
            &connection,
            query,
            16 * 1024 * 1024,
            QueryErrorKind::Resource,
            &|| false,
        );
        let checks = Cell::new(0);
        let is_interrupted = || {
            checks.set(checks.get() + 1);
            checks.get() >= 64
        };
        assert_seek_folding_failure(
            &connection,
            query,
            256 * 1024 * 1024,
            QueryErrorKind::Interrupted,
            &is_interrupted,
        );
        assert_eq!(checks.get(), 64, "query: {query}");
    }
}

#[test]
fn batches_keep_memory_leases_after_later_advancement_and_cursor_close() {
    let connection = database();
    let prepared = prepare(
        &connection,
        "UNWIND range(1,3) AS i RETURN range(1,2000) AS payload",
        BTreeMap::new(),
        options(16 * 1024 * 1024),
    )
    .unwrap();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let first = cursor.next_batch(&connection, 1).unwrap();
    let first_bytes = lithograph_core::query::resources::owned_row_bytes(&first.rows[0]).unwrap();
    let before_second = resources.used();
    let second = cursor.next_batch(&connection, 1).unwrap();
    let second_bytes = lithograph_core::query::resources::owned_row_bytes(&second.rows[0]).unwrap();
    assert!(
        resources.used() >= before_second + second_bytes,
        "advancing cannot release the first batch's live output"
    );
    cursor.cancel(&connection).unwrap();
    drop(cursor);
    assert!(
        resources.used() >= first_bytes + second_bytes,
        "output ownership survives cursor cancellation and drop"
    );
    let both_used = resources.used();
    drop(first);
    assert!(resources.used() <= both_used - first_bytes);
    assert!(resources.used() >= second_bytes);
    drop(second);
    assert_eq!(resources.used(), 0);
}

#[test]
fn sorted_cursor_early_close_releases_retained_inputs_and_spill_resources() {
    let connection = database();
    let prepared = prepare(
        &connection,
        "UNWIND range(1,2000) AS i RETURN i ORDER BY i DESC",
        BTreeMap::new(),
        options(16 * 1024 * 1024),
    )
    .unwrap();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let first = cursor.next_batch(&connection, 1).unwrap();
    assert_eq!(first.rows, vec![vec![Value::Integer(2000)]]);
    drop(first);
    cursor.cancel(&connection).unwrap();
    assert_eq!(resources.used(), 0);
    drop(cursor);
    assert_eq!(resources.used(), 0);
}

#[test]
fn profile_runtime_allocation_and_summary_copies_share_the_execution_budget() {
    let connection = database();
    let mut query = String::from("PROFILE ");
    for index in 0..1024 {
        query.push_str(&format!("MATCH (n{index}) "));
    }
    query.push_str("RETURN count(n0) AS count");
    let resources = QueryResources::new(Some(32 * 1024));
    let error = prepare_with_resources_and_interrupt(
        &connection,
        &query,
        BTreeMap::new(),
        options(32 * 1024),
        resources.clone(),
        &|| false,
    )
    .expect_err("PROFILE must preflight runtime arrays before cursor construction");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(resources.used(), 0);

    let prepared = prepare(&connection, &query, BTreeMap::new(), options(1024 * 1024)).unwrap();
    let resources = prepared.resources.clone();
    assert!(resources.used() > 32 * 1024);
    let mut cursor = QueryCursor::new(prepared);
    let batch = cursor.next_batch(&connection, 1).unwrap();
    let summary = batch.summary.as_ref().expect("profile summary");
    assert!(summary.metrics.operator_profile().unwrap().len() >= 1024);
    cursor.cancel(&connection).unwrap();
    drop(cursor);
    assert!(
        resources.used() > 32 * 1024,
        "summary owns its live profile copy"
    );
    drop(batch);
    assert_eq!(resources.used(), 0);
}
