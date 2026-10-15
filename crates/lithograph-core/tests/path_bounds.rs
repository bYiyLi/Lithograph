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
    execute(
        &connection,
        "CREATE (a:BoundNode {id:1})-[:R]->(b:BoundNode {id:2})-[:R]->(:BoundNode {id:3}) FINISH",
    );
    connection
}

fn execute(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap_or_else(|error| panic!("prepare {query:?}: {error}"));
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor
            .next_batch(connection, 2)
            .unwrap_or_else(|error| panic!("execute {query:?}: {error}"));
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection).expect("complete");
            return rows;
        }
    }
}

#[test]
fn legacy_empty_intervals_produce_no_matches_without_side_effects() {
    let connection = graph();
    // Inherited Match5.feature scenarios 11/12/13 define these as empty matches.
    for query in [
        "MATCH (a:BoundNode) MATCH (a)-[:R*2..1]->(b) RETURN b.id",
        "MATCH (a:BoundNode) MATCH (a)-[:R*1..0]->(b) RETURN b.id",
        "MATCH (a:BoundNode) MATCH (a)-[:R*..0]->(b) RETURN b.id",
    ] {
        assert!(execute(&connection, query).is_empty(), "{query}");
    }
    assert_eq!(
        execute(&connection, "MATCH ()-[r:R]->() RETURN count(r)"),
        [vec![Value::Integer(2)]]
    );
}

#[test]
fn modern_quantified_path_rejects_reversed_bounds() {
    let connection = graph();
    let prepared = prepare(
        &connection,
        "MATCH p=(a)-[:R]->{3,1}(b) RETURN length(p)",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("valid syntax");
    let error = QueryCursor::new(prepared)
        .next_batch(&connection, 2)
        .expect_err("modern upper bound smaller than lower bound must fail");
    assert_eq!(error.kind, QueryErrorKind::Semantic);
}

#[test]
fn zero_length_is_valid_when_both_bounds_include_zero() {
    let connection = graph();
    for query in [
        "MATCH p=(a:BoundNode)-[:R*0..0]->(b) RETURN length(p)",
        "MATCH p=(a:BoundNode)-[:R]->{0,0}(b) RETURN length(p)",
    ] {
        assert_eq!(
            execute(&connection, query),
            [
                vec![Value::Integer(0)],
                vec![Value::Integer(0)],
                vec![Value::Integer(0)]
            ],
            "{query}"
        );
    }
}
