use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite must open");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                id INTEGER PRIMARY KEY CHECK(id=1),\
                magic TEXT NOT NULL,\
                database_id TEXT NOT NULL,\
                storage_format INTEGER NOT NULL\
             );",
        )
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn commit_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("commit count")
}

fn execute_rows(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare query");
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 32).expect("query batch");
        rows.extend(batch.rows);
        if batch.done {
            break;
        }
    }
    rows
}

fn execute_complete(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare query");
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 32).expect("query batch");
        rows.extend(batch.rows);
        if batch.done {
            break;
        }
    }
    cursor.complete(connection).expect("complete query");
    rows
}

fn local_file_uri(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        format!("file:///{}", normalized.trim_start_matches('/'))
    } else {
        format!("file://{normalized}")
    }
}

fn csv_fixture(contents: &str) -> (PathBuf, String) {
    let path = std::env::temp_dir().join(format!(
        "lithograph_phase15_stream_{}_{}.csv",
        std::process::id(),
        contents.len()
    ));
    fs::write(&path, contents).expect("write CSV fixture");
    let uri = local_file_uri(&path);
    (path, uri)
}

#[test]
fn transaction_stream_does_not_execute_future_batches_before_consumption() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "UNWIND [1,2,3] AS value \
         CALL (value) { CREATE (:StreamBatch {value:value}) } \
         IN TRANSACTIONS OF 1 ROWS \
         RETURN value",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare transaction stream");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first transaction stream row");
    assert_eq!(first.rows, vec![vec![Value::Integer(1)]]);
    assert!(!first.done);
    assert_eq!(commit_count(&connection), before + 1);

    cursor
        .cancel(&connection)
        .expect("cancel transaction stream");
    assert_eq!(commit_count(&connection), before + 1);
    assert_eq!(
        execute_rows(
            &connection,
            "MATCH (n:StreamBatch) RETURN n.value ORDER BY n.value"
        ),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn load_csv_transaction_prefix_is_pulled_one_committed_batch_at_a_time() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let (path, uri) = csv_fixture("name\nalpha\nbeta\ngamma\n");
    let query = format!(
        "LOAD CSV WITH HEADERS FROM '{uri}' AS row \
         CALL (row) {{ CREATE (:CsvStreamBatch {{name:row.name}}) }} \
         IN TRANSACTIONS OF 1 ROWS \
         RETURN 1 AS seen"
    );
    let prepared = prepare(
        &connection,
        &query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare LOAD CSV transaction stream");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first LOAD CSV transaction stream row");
    assert_eq!(first.rows, vec![vec![Value::Integer(1)]]);
    assert!(!first.done);
    assert_eq!(commit_count(&connection), before + 1);

    cursor
        .cancel(&connection)
        .expect("cancel LOAD CSV transaction stream");
    assert_eq!(commit_count(&connection), before + 1);
    assert_eq!(
        execute_rows(
            &connection,
            "MATCH (n:CsvStreamBatch) RETURN n.name ORDER BY n.name"
        ),
        vec![vec![Value::String("alpha".to_owned())]]
    );

    fs::remove_file(path).expect("remove CSV fixture");
}

#[test]
fn call_subquery_stream_exposes_earlier_outer_rows_before_late_failure() {
    let connection = fresh_storage();
    let prepared = prepare(
        &connection,
        "UNWIND [1, 0] AS x CALL (x) { RETURN range(1, 2, x) AS values } RETURN values",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare CALL subquery stream");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first CALL subquery row must be available before the second outer input");
    assert_eq!(
        first.rows,
        vec![vec![Value::List(vec![
            Value::Integer(1),
            Value::Integer(2),
        ])]]
    );
    assert!(!first.done);

    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("zero range step must fail only when the second outer row is pulled");
    assert!(
        error.to_string().contains("step") || error.to_string().contains("zero"),
        "unexpected late CALL subquery error: {error}"
    );
}

#[test]
fn union_all_stream_exposes_first_branch_before_late_branch_failure() {
    let connection = fresh_storage();
    let prepared = prepare(
        &connection,
        "{ RETURN range(1, 2, 1) AS values } UNION ALL { RETURN range(1, 2, 0) AS values }",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare UNION ALL stream");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first UNION branch must be visible before evaluating the second branch");
    assert_eq!(
        first.rows,
        vec![vec![Value::List(vec![
            Value::Integer(1),
            Value::Integer(2),
        ])]]
    );
    assert!(!first.done);

    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("second UNION branch must fail only when it is pulled");
    assert!(
        error.to_string().contains("step") || error.to_string().contains("zero"),
        "unexpected late UNION error: {error}"
    );
}

#[test]
fn next_stream_pulls_previous_rows_incrementally() {
    let connection = fresh_storage();
    let prepared = prepare(
        &connection,
        "{ UNWIND [1, 0] AS x RETURN x } NEXT RETURN range(1, 2, x) AS values",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare NEXT stream");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("NEXT must expose the first transformed row before pulling the second input");
    assert_eq!(
        first.rows,
        vec![vec![Value::List(vec![
            Value::Integer(1),
            Value::Integer(2),
        ])]]
    );
    assert!(!first.done);

    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("NEXT must fail only when the second input row is pulled");
    assert!(
        error.to_string().contains("step") || error.to_string().contains("zero"),
        "unexpected late NEXT error: {error}"
    );
}

#[test]
fn transaction_order_by_defers_rows_until_all_batches_commit() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "UNWIND [3,1,2] AS value \
         CALL (value) { CREATE (:OrderedBatch {value:value}) } \
         IN TRANSACTIONS OF 1 ROWS \
         RETURN value ORDER BY value",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare ordered transaction stream");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first ordered transaction row");
    assert_eq!(first.rows, vec![vec![Value::Integer(1)]]);
    assert!(!first.done);
    assert_eq!(commit_count(&connection), before + 3);

    let mut values = first.rows;
    loop {
        let batch = cursor
            .next_batch(&connection, 1)
            .expect("ordered transaction row");
        values.extend(batch.rows);
        if batch.done {
            break;
        }
    }
    cursor
        .complete(&connection)
        .expect("complete ordered stream");
    assert_eq!(
        values,
        vec![
            vec![Value::Integer(1)],
            vec![Value::Integer(2)],
            vec![Value::Integer(3)]
        ]
    );
}

#[test]
fn concurrent_disjoint_transaction_stream_still_advances_one_batch_at_a_time() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "UNWIND [1,2,3] AS value \
         CALL (value) { CREATE (:DisjointStream {value:value}) } \
         IN 2 CONCURRENT TRANSACTIONS OF 1 ROWS DISJOINT BY (value) \
         RETURN value",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare DISJOINT transaction stream");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first DISJOINT transaction row");
    assert_eq!(first.rows, vec![vec![Value::Integer(1)]]);
    assert!(!first.done);
    assert_eq!(commit_count(&connection), before + 1);

    cursor
        .cancel(&connection)
        .expect("cancel DISJOINT transaction stream");
    assert_eq!(commit_count(&connection), before + 1);
    assert_eq!(
        execute_rows(
            &connection,
            "MATCH (n:DisjointStream) RETURN n.value ORDER BY n.value"
        ),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn mutating_return_cancel_rolls_back_the_unpublished_write() {
    let connection = fresh_storage();
    execute_complete(
        &connection,
        "CREATE (:WriteSeed {value:1}), (:WriteSeed {value:2}), (:WriteSeed {value:3}) FINISH",
    );
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "MATCH (n:WriteSeed) \
         CREATE (:WriteStream {value:n.value}) \
         RETURN n.value ORDER BY n.value",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare mutating stream");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first mutating row");
    assert_eq!(first.rows, vec![vec![Value::Integer(1)]]);
    assert!(!first.done);
    assert_eq!(commit_count(&connection), before + 1);

    cursor.cancel(&connection).expect("cancel mutating stream");
    assert_eq!(commit_count(&connection), before);
    assert_eq!(
        execute_rows(&connection, "MATCH (n:WriteStream) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
}

#[test]
fn late_mutating_projection_failure_rolls_back_the_prepared_commit() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "CREATE (:LateProjection {value:1}) RETURN range(1, 2, 0) AS values",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare late-failing mutating projection");
    let mut cursor = QueryCursor::new(prepared);

    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("projection runtime failure must abort the unpublished write");
    assert!(
        error.to_string().contains("step") || error.to_string().contains("zero"),
        "unexpected projection error: {error}"
    );
    assert_eq!(commit_count(&connection), before);
    assert_eq!(
        execute_rows(&connection, "MATCH (n:LateProjection) RETURN count(n)"),
        vec![vec![Value::Integer(0)]]
    );
    assert_eq!(
        execute_rows(&connection, "RETURN 1"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn direct_mutating_projection_streams_across_batches_with_skip_and_limit() {
    let connection = fresh_storage();
    execute_complete(
        &connection,
        "CREATE (:LazyWrite {value:1}), (:LazyWrite {value:2}), \
                (:LazyWrite {value:3}), (:LazyWrite {value:4}), \
                (:LazyWrite {value:5}) FINISH",
    );
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "MATCH (n:LazyWrite) \
         SET n.streamed = true \
         RETURN n.value SKIP 1 LIMIT 3",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare direct mutating projection");
    let mut cursor = QueryCursor::new(prepared);

    let mut rows = Vec::new();
    for expected_done in [false, false, true] {
        let batch = cursor
            .next_batch(&connection, 1)
            .expect("direct mutating projection batch");
        assert_eq!(batch.rows.len(), 1);
        assert_eq!(batch.done, expected_done);
        rows.extend(batch.rows);
    }
    cursor
        .complete(&connection)
        .expect("complete direct mutating projection");
    assert_eq!(rows.len(), 3);
    assert_eq!(commit_count(&connection), before + 1);
    assert_eq!(
        execute_rows(
            &connection,
            "MATCH (n:LazyWrite) WHERE n.streamed = true RETURN count(n)"
        ),
        vec![vec![Value::Integer(5)]]
    );
}

#[test]
fn direct_mutating_projection_limit_zero_commits_without_result_rows() {
    let connection = fresh_storage();
    execute_complete(&connection, "CREATE (:LazyZero {value:1}) FINISH");
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "MATCH (n:LazyZero) SET n.updated = true RETURN n.value LIMIT 0",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare zero-limit mutating projection");
    let mut cursor = QueryCursor::new(prepared);

    let batch = cursor
        .next_batch(&connection, 1)
        .expect("zero-limit mutating projection");
    assert!(batch.rows.is_empty());
    assert!(batch.done);
    cursor
        .complete(&connection)
        .expect("complete zero-limit mutating projection");
    assert_eq!(commit_count(&connection), before + 1);
    assert_eq!(
        execute_rows(&connection, "MATCH (n:LazyZero) RETURN n.updated"),
        vec![vec![Value::Boolean(true)]]
    );
}

#[test]
fn mutating_order_by_projection_uses_barrier_without_losing_commit_semantics() {
    let connection = fresh_storage();
    execute_complete(
        &connection,
        "CREATE (:BarrierWrite {value:3}), (:BarrierWrite {value:1}), \
                (:BarrierWrite {value:2}), (:BarrierWrite {value:4}) FINISH",
    );
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "MATCH (n:BarrierWrite) \
         SET n.ordered = true \
         RETURN n.value ORDER BY n.value DESC SKIP 1 LIMIT 2",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare ordered mutating projection");
    let mut cursor = QueryCursor::new(prepared);

    let first = cursor
        .next_batch(&connection, 1)
        .expect("first ordered mutating row");
    assert_eq!(first.rows, vec![vec![Value::Integer(3)]]);
    assert!(!first.done);
    let second = cursor
        .next_batch(&connection, 1)
        .expect("second ordered mutating row");
    assert_eq!(second.rows, vec![vec![Value::Integer(2)]]);
    assert!(second.done);
    cursor
        .complete(&connection)
        .expect("complete ordered mutating projection");
    assert_eq!(commit_count(&connection), before + 1);
}

#[test]
fn mutating_distinct_projection_deduplicates_through_spill_barrier() {
    let connection = fresh_storage();
    execute_complete(
        &connection,
        "CREATE (:DistinctWrite {group:1}), (:DistinctWrite {group:1}), \
                (:DistinctWrite {group:2}) FINISH",
    );
    let prepared = prepare(
        &connection,
        "MATCH (n:DistinctWrite) \
         SET n.seen = true \
         RETURN DISTINCT n.group",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare distinct mutating projection");
    let mut cursor = QueryCursor::new(prepared);

    let mut rows = Vec::new();
    loop {
        let batch = cursor
            .next_batch(&connection, 1)
            .expect("distinct mutating projection batch");
        rows.extend(batch.rows);
        if batch.done {
            break;
        }
    }
    cursor
        .complete(&connection)
        .expect("complete distinct mutating projection");
    rows.sort_by_key(|row| match row.first() {
        Some(Value::Integer(value)) => *value,
        other => panic!("unexpected distinct row: {other:?}"),
    });
    assert_eq!(rows, vec![vec![Value::Integer(1)], vec![Value::Integer(2)]]);
}

#[test]
fn mutating_count_projection_materializes_only_the_aggregate_result() {
    let connection = fresh_storage();
    execute_complete(
        &connection,
        "CREATE (:AggregateWrite {value:1}), (:AggregateWrite {value:2}), \
                (:AggregateWrite {value:3}) FINISH",
    );
    let prepared = prepare(
        &connection,
        "MATCH (n:AggregateWrite) SET n.counted = true RETURN count(n) AS total",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare aggregate mutating projection");
    let mut cursor = QueryCursor::new(prepared);

    let batch = cursor
        .next_batch(&connection, 1)
        .expect("aggregate mutating projection");
    assert_eq!(batch.rows, vec![vec![Value::Integer(3)]]);
    assert!(batch.done);
    cursor
        .complete(&connection)
        .expect("complete aggregate mutating projection");
}
