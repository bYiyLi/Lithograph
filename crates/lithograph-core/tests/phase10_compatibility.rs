use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, prepare,
};
use lithograph_core::storage::{
    STORAGE_FORMAT, branch_head, create_storage_schema, initialize_connection_state,
    initialize_root, list_tags,
};
use rusqlite::Connection;

fn fresh_storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                 id INTEGER PRIMARY KEY CHECK(id=1),\
                 magic TEXT NOT NULL,\
                 database_id TEXT NOT NULL,\
                 storage_format INTEGER NOT NULL\
             );",
        )
        .expect("metadata");
    connection
        .execute(
            "INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format) \
             VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000012', ?1)",
            [STORAGE_FORMAT],
        )
        .expect("metadata marker");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    initialize_connection_state(&connection).expect("connection state");
    connection
}

fn execute(
    connection: &Connection,
    query: &str,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 64)?;
        rows.extend(batch.rows);
        if batch.done {
            return cursor.complete(connection).map(|summary| (rows, summary));
        }
    }
}

#[test]
fn explain_validates_and_plans_without_write_or_version_side_effects() {
    let connection = fresh_storage();
    let root = branch_head(&connection, "main").expect("root head");

    let (write_plan, write_summary) =
        execute(&connection, "EXPLAIN CREATE (:Explained) FINISH").expect("EXPLAIN write");
    assert_eq!(write_summary.metrics.db_hits, 0);
    assert!(write_plan.iter().flatten().any(|value| {
        matches!(value, Value::String(plan) if plan.contains("Mutation") || plan.contains("Write"))
    }));
    assert_eq!(branch_head(&connection, "main").expect("head"), root);

    let version = format!(
        "EXPLAIN CALL lithograph.tag.create('explained', 'commit/{}') YIELD name RETURN name",
        root.to_hex()
    );
    let (_, version_summary) = execute(&connection, &version).expect("EXPLAIN version operation");
    assert_eq!(version_summary.metrics.db_hits, 0);
    assert!(list_tags(&connection).expect("Tags").is_empty());

    let (rows, _) = execute(&connection, "MATCH (n:Explained) RETURN count(n)").expect("read");
    assert_eq!(rows, vec![vec![Value::Integer(0)]]);
}

#[test]
fn profile_preserves_results_and_collects_execution_metrics() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Person {name:'Alice'})-[:KNOWS]->(:Person {name:'Bob'}) FINISH",
    )
    .expect("seed");
    let query = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name, b.name";
    let (normal_rows, normal_summary) = execute(&connection, query).expect("normal query");
    let (profile_rows, profile_summary) =
        execute(&connection, &format!("PROFILE {query}")).expect("PROFILE query");

    assert_eq!(profile_rows, normal_rows);
    assert!(normal_summary.metrics.operator_profile().is_none());
    assert_eq!(profile_summary.metrics.rows, profile_rows.len() as u64);
    assert!(profile_summary.metrics.db_hits > 0);
    let operators = profile_summary
        .metrics
        .operator_profile()
        .expect("PROFILE operator metrics");
    assert_eq!(
        operators
            .iter()
            .map(|operator| operator.id)
            .collect::<Vec<_>>(),
        (0..operators.len() as u64).collect::<Vec<_>>()
    );
    assert!(operators.iter().any(|operator| {
        matches!(operator.operator.as_str(), "LabelIndexScan" | "IndexSeek")
            && operator.rows > 0
            && operator.db_hits > 0
    }));
    assert_eq!(
        operators
            .iter()
            .map(|operator| operator.db_hits)
            .sum::<u64>(),
        profile_summary.metrics.db_hits
    );
    assert_eq!(
        operators.last().map(|operator| operator.rows),
        Some(profile_summary.metrics.rows)
    );
}

#[test]
fn mutation_without_final_projection_does_not_expose_internal_bindings() {
    let connection = fresh_storage();

    let (rows, summary) =
        execute(&connection, "CREATE (n:Hidden {value: 42})").expect("mutation without RETURN");
    assert!(rows.is_empty());
    assert_eq!(summary.metrics.rows, 0);
    assert_eq!(summary.counters.nodes_created, 1);
    assert_eq!(summary.counters.properties_set, 1);

    let (rows, projected_summary) = execute(
        &connection,
        "CREATE (n:Visible {value: 7}) RETURN n.value AS value",
    )
    .expect("mutation with RETURN");
    assert_eq!(rows, vec![vec![Value::Integer(7)]]);
    assert_eq!(projected_summary.metrics.rows, 1);

    let (rows, _) = execute(&connection, "MATCH (n) RETURN count(n)").expect("verify writes");
    assert_eq!(rows, vec![vec![Value::Integer(2)]]);
}

#[test]
fn compatibility_errors_preserve_categories_positions_and_busy_primary_codes() {
    let connection = fresh_storage();
    for (query, expected) in [
        ("RETURN 1,\n", QueryErrorKind::Parse),
        ("MATCH (n)\nWITH n AS x\nRETURN n", QueryErrorKind::Semantic),
        ("RETURN\n'x' + 1", QueryErrorKind::Type),
    ] {
        let error = prepare(
            &connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect_err("query must fail validation");
        assert_eq!(error.kind, expected);
        assert!(error.line.is_some_and(|line| line > 0));
        assert!(error.column.is_some_and(|column| column > 0));
    }

    execute(
        &connection,
        "CREATE (:Person {name:'A'})-[:KNOWS]->(:Person {name:'B'}) FINISH",
    )
    .expect("seed connected Nodes");
    let constraint = execute(
        &connection,
        "MATCH (n:Person) WHERE n.name = 'A' DELETE n FINISH",
    )
    .expect_err("connected Node delete must fail");
    assert_eq!(constraint.kind, QueryErrorKind::Constraint);

    for code in [rusqlite::ffi::SQLITE_BUSY, rusqlite::ffi::SQLITE_LOCKED] {
        let sqlite = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
        let mapped = QueryError::from(sqlite);
        assert_eq!(mapped.kind, QueryErrorKind::Busy);
        assert_eq!(mapped.sqlite_code, Some(code));
    }
}
