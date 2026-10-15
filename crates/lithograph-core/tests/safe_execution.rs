use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, prepare};
use lithograph_core::storage::{branch_head, create_storage_schema, initialize_root};
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
        .expect("metadata table");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn options(text: &str) -> ExecutionOptions {
    ExecutionOptions::parse_text(text).expect("valid options")
}

fn execute(connection: &Connection, query: &str, options: ExecutionOptions) -> Vec<Vec<Value>> {
    let prepared = prepare(connection, query, BTreeMap::new(), options).expect("prepare");
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 16).expect("next batch");
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection).expect("complete");
            return rows;
        }
    }
}

#[test]
fn safe_mode_preserves_graph_history_and_composed_reads() {
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:Safe {v:7}) FINISH",
        ExecutionOptions::default(),
    );
    let head = branch_head(&connection, "main").expect("head");
    let safe = options(r#"{"readOnlyNoExternalIO":true}"#);
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:Safe) CALL (n) { RETURN n.v AS v } RETURN v",
            safe.clone(),
        ),
        vec![vec![Value::Integer(7)]]
    );
    assert_eq!(
        execute(
            &connection,
            "CALL lithograph.branch.list() YIELD name RETURN name",
            safe.clone(),
        ),
        vec![vec![Value::String("main".to_owned())]]
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:Safe) RETURN count(n)",
            options(&format!(
                r#"{{"at":"commit/{}","readOnlyNoExternalIO":true}}"#,
                head.to_hex()
            )),
        ),
        vec![vec![Value::Integer(1)]]
    );
    assert_eq!(branch_head(&connection, "main").expect("head"), head);
}

#[test]
fn safe_mode_rejects_all_known_side_effect_paths_during_prepare() {
    let connection = fresh_storage();
    let head = branch_head(&connection, "main").expect("head");
    let safe = options(r#"{"readOnlyNoExternalIO":true}"#);
    for query in [
        "CREATE (:Blocked) FINISH",
        "UNWIND [] AS x CREATE (:Blocked) FINISH",
        "EXPLAIN CREATE (:Blocked) FINISH",
        "PROFILE CREATE (:Blocked) FINISH",
        "UNWIND [] AS x CALL lithograph.branch.create('blocked') YIELD name RETURN name",
        "EXPLAIN CALL lithograph.branch.checkout('main') YIELD name RETURN name",
        "WHEN false THEN { CREATE (:Blocked) RETURN 1 AS x } ELSE { RETURN 1 AS x }",
        "CALL { CREATE (:Blocked) }",
        "CALL { LOAD CSV FROM 'file:///__lithograph_safe_mode_forbidden__.csv' AS row RETURN row } RETURN 1",
        "CREATE RANGE INDEX blocked FOR (n:Blocked) ON (n.v)",
        "CALL lithograph.branch.create('blocked') YIELD name RETURN name",
        "CALL lithograph.branch.checkout('main') YIELD name RETURN name",
        "CALL lithograph.index.rebuild('missing')",
        "UNWIND [1] AS x CALL (x) { RETURN x AS y } IN TRANSACTIONS RETURN y",
        "LOAD CSV FROM 'file:///__lithograph_safe_mode_forbidden__.csv' AS row RETURN row",
        "LOAD CSV FROM 'http://127.0.0.1:9/forbidden.csv' AS row RETURN row",
        "CALL db.index.semantic.queryNodes('missing','text',{limit:1}) YIELD node RETURN node",
        "CALL db.index.semantic.rebuild('missing','branch/main') YIELD name RETURN name",
        "CALL db.index.fulltext.queryNodes('missing','text') YIELD node RETURN node",
    ] {
        let error = prepare(&connection, query, BTreeMap::new(), safe.clone()).expect_err(query);
        assert_eq!(
            error.kind,
            QueryErrorKind::InvalidArgument,
            "{query}: {error}"
        );
        assert!(
            error
                .message
                .contains("options.readOnlyNoExternalIO forbids"),
            "{query}: {error}"
        );
    }
    let mut params = BTreeMap::new();
    params.insert(
        "source".to_owned(),
        Value::String("file:///__lithograph_safe_mode_forbidden__.csv".to_owned()),
    );
    let error = prepare(
        &connection,
        "LOAD CSV FROM $source AS row RETURN row",
        params,
        safe,
    )
    .expect_err("parameterized LOAD CSV must be rejected");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert_eq!(branch_head(&connection, "main").expect("head"), head);
}

#[test]
fn safe_option_requires_a_boolean_and_false_keeps_default_behavior() {
    for invalid in [
        r#"{"readOnlyNoExternalIO":null}"#,
        r#"{"readOnlyNoExternalIO":"true"}"#,
        r#"{"readOnlyNoExternalIO":1}"#,
    ] {
        assert_eq!(
            ExecutionOptions::parse_text(invalid)
                .expect_err("non-Boolean safe option")
                .kind,
            QueryErrorKind::InvalidArgument
        );
    }
    let connection = fresh_storage();
    execute(
        &connection,
        "CREATE (:DefaultStillWritable) FINISH",
        options(r#"{"readOnlyNoExternalIO":false}"#),
    );
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:DefaultStillWritable) RETURN count(n)",
            options(r#"{"readOnlyNoExternalIO":true}"#),
        ),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn safe_mode_checks_interpolated_subqueries_before_execution() {
    let connection = fresh_storage();
    let head = branch_head(&connection, "main").expect("head");
    let safe = options(r#"{"readOnlyNoExternalIO":true}"#);
    for body in [
        "RETURN s\"{COUNT { LOAD CSV FROM 'file:///__lithograph_safe_interpolation__.csv' AS row RETURN row }}\" AS x",
        "RETURN s\"{COUNT { CALL db.index.semantic.queryNodes('missing','text',{limit:1}) YIELD node RETURN node }}\" AS x",
        "RETURN s\"{COUNT { CALL db.index.fulltext.queryNodes('missing','text') YIELD node RETURN node }}\" AS x",
        "RETURN s\"{COUNT { CALL lithograph.branch.create('blocked') YIELD name RETURN name }}\" AS x",
        "RETURN CASE WHEN false THEN s\"{COUNT { LOAD CSV FROM 'file:///__lithograph_safe_interpolation__.csv' AS row RETURN row }}\" ELSE 'safe' END AS x",
    ] {
        for prefix in ["", "EXPLAIN ", "PROFILE "] {
            let source = format!("{prefix}{body}");
            let error =
                prepare(&connection, &source, BTreeMap::new(), safe.clone()).expect_err(&source);
            assert_eq!(
                error.kind,
                QueryErrorKind::InvalidArgument,
                "{source}: {error}"
            );
            assert!(
                error
                    .message
                    .contains("options.readOnlyNoExternalIO forbids"),
                "{source}: {error}"
            );
        }
    }
    assert_eq!(
        execute(&connection, "RETURN s\"safe={1 + 2}\" AS text", safe,),
        vec![vec![Value::String("safe=3".to_owned())]],
    );
    assert_eq!(branch_head(&connection, "main").expect("head"), head);
}
