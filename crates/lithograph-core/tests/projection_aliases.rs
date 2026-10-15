use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, prepare};
use lithograph_core::storage::{create_storage_schema, initialize_root};
use rusqlite::Connection;

fn graph() -> Connection {
    let connection = Connection::open_in_memory().expect("open graph");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1),\
             magic TEXT NOT NULL,\
             database_id TEXT NOT NULL,\
             storage_format INTEGER NOT NULL);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("schema");
    initialize_root(&connection).expect("root");
    connection
}

fn result(connection: &Connection, query: &str) -> (Vec<String>, Vec<Vec<Value>>) {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap_or_else(|error| panic!("prepare {query:?}: {error}"));
    let columns = prepared.columns.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 2).expect("batch");
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection).expect("complete");
            return (columns, rows);
        }
    }
}

#[test]
fn outer_projection_alias_owns_collect_result_column_and_following_scope() {
    let connection = graph();
    let (columns, rows) = result(
        &connection,
        "RETURN COLLECT { UNWIND [1,2] AS n RETURN n AS innerValue } AS values",
    );
    assert_eq!(columns, ["values"]);
    assert_eq!(
        rows,
        [vec![Value::List(vec![
            Value::Integer(1),
            Value::Integer(2)
        ])]]
    );
    let (columns, rows) = result(
        &connection,
        "WITH COLLECT { RETURN 3 AS innerValue } AS values \
         RETURN values NEXT RETURN size(values) AS count",
    );
    assert_eq!(columns, ["count"]);
    assert_eq!(rows, [vec![Value::Integer(1)]]);
}

#[test]
fn nested_collect_aliases_remain_local_at_each_projection_boundary() {
    let connection = graph();
    let (columns, rows) = result(
        &connection,
        "RETURN COLLECT { \
           RETURN COLLECT { RETURN 1 AS innerValue } AS middleValue \
         } AS outerValue",
    );
    assert_eq!(columns, ["outerValue"]);
    assert_eq!(
        rows,
        [vec![Value::List(vec![Value::List(vec![Value::Integer(
            1
        )])])]]
    );
    let (columns, rows) = result(
        &connection,
        "RETURN EXISTS { RETURN 1 AS innerValue } AS present, \
         COUNT { RETURN 1 AS innerValue } AS total",
    );
    assert_eq!(columns, ["present", "total"]);
    assert_eq!(rows, [vec![Value::Boolean(true), Value::Integer(1)]]);
}

#[test]
fn inner_alias_does_not_supply_a_missing_outer_projection_alias() {
    let connection = graph();
    let expression = "COLLECT { RETURN 1 AS innerValue }";
    let (columns, rows) = result(&connection, &format!("RETURN {expression}"));
    assert_eq!(columns, [expression]);
    assert_eq!(rows, [vec![Value::List(vec![Value::Integer(1)])]]);
    for query in [
        "WITH COLLECT { RETURN 1 AS innerValue } RETURN innerValue",
        "RETURN COLLECT { RETURN 1 AS innerValue } NEXT RETURN innerValue",
    ] {
        let error = prepare(
            &connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect_err("inner alias must not satisfy outer WITH/NEXT alias requirement");
        assert_eq!(error.kind, QueryErrorKind::Semantic);
    }
}

#[test]
fn collect_projection_modifiers_do_not_change_outer_order_distinct_or_pagination() {
    let connection = graph();
    let (columns, rows) = result(
        &connection,
        "UNWIND [2,1,1] AS x \
         RETURN x, COLLECT { \
           UNWIND [3,1,2,2] AS n \
           RETURN DISTINCT n AS innerValue ORDER BY innerValue DESC SKIP 1 LIMIT 1 \
         } AS values ORDER BY x ASC",
    );
    assert_eq!(columns, ["x", "values"]);
    assert_eq!(
        rows,
        [
            vec![Value::Integer(1), Value::List(vec![Value::Integer(2)])],
            vec![Value::Integer(1), Value::List(vec![Value::Integer(2)])],
            vec![Value::Integer(2), Value::List(vec![Value::Integer(2)])],
        ]
    );
    let (columns, rows) = result(
        &connection,
        "UNWIND [1,2] AS x \
         RETURN x, COLLECT { UNWIND [2,1] AS n RETURN n AS innerValue ORDER BY innerValue } AS values",
    );
    assert_eq!(columns, ["x", "values"]);
    assert_eq!(
        rows,
        [
            vec![
                Value::Integer(1),
                Value::List(vec![Value::Integer(1), Value::Integer(2)])
            ],
            vec![
                Value::Integer(2),
                Value::List(vec![Value::Integer(1), Value::Integer(2)])
            ],
        ]
    );
}
