use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, prepare};
use lithograph_core::storage::{branch_head, create_storage_schema, initialize_root};
use rusqlite::Connection;

fn storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn rows(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect(query);
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 16).expect("query batch");
        rows.extend(batch.rows);
        if batch.done {
            break;
        }
    }
    cursor.complete(connection).expect("complete");
    rows
}

#[test]
fn interpolation_keeps_external_io_and_maintenance_classification() {
    let connection = storage();
    for (query, maintenance) in [
        (
            "RETURN s\"{COUNT { LOAD CSV FROM 'file:///__phase18_classification__.csv' AS row RETURN row }}\"",
            false,
        ),
        (
            "RETURN s\"{COUNT { CALL db.index.semantic.queryNodes('missing','text',{limit:1}) YIELD node RETURN node }}\"",
            false,
        ),
        (
            "RETURN s\"{COUNT { CALL db.index.semantic.rebuild('missing','branch/main') YIELD name RETURN name }}\"",
            true,
        ),
        (
            "RETURN CASE WHEN false THEN s\"{COUNT { CALL db.index.semantic.rebuild('missing','branch/main') YIELD name RETURN name }}\" ELSE 'safe' END",
            true,
        ),
    ] {
        let prepared = prepare(
            &connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect(query);
        assert!(
            prepared.has_external_io(),
            "external I/O classification: {query}"
        );
        assert_eq!(
            prepared.has_semantic_maintenance(),
            maintenance,
            "maintenance classification: {query}"
        );
    }
    let prepared = prepare(
        &connection,
        "RETURN s\"{COUNT { RETURN 1 }}\"",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("pure interpolation");
    assert!(!prepared.has_external_io());
    assert!(!prepared.has_semantic_maintenance());
}

#[test]
fn semantic_calls_inside_interpolation_keep_transaction_owning_restriction() {
    let connection = storage();
    for body in [
        "CALL db.index.semantic.queryNodes('missing','text',{limit:1}) YIELD node RETURN node",
        "CALL db.index.semantic.queryRelationships('missing','text',{limit:1}) YIELD relationship RETURN relationship",
        "CALL db.index.semantic.rebuild('missing','branch/main') YIELD name RETURN name",
    ] {
        let query = format!(
            "UNWIND [1] AS x CALL (x) {{ RETURN s\"{{COUNT {{ {body} }}}}\" AS count }} IN TRANSACTIONS RETURN count"
        );
        let error = prepare(
            &connection,
            &query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )
        .expect_err("Semantic procedure cannot run inside independent transaction batches");
        assert_eq!(
            error.kind,
            QueryErrorKind::TransactionBoundaryRequired,
            "{query}: {error}"
        );
    }
}

#[test]
fn interpolation_version_procedures_keep_write_and_lifecycle_classification() {
    let connection = storage();
    let query = "RETURN s\"{COUNT { CALL lithograph.branch.create('interpolated') YIELD name RETURN name }}\"";
    let prepared = prepare(
        &connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("valid Version interpolation");
    let mut cursor = QueryCursor::new(prepared.clone());
    let batch = cursor
        .next_batch(&connection, 16)
        .expect("actual branch creation");
    assert_eq!(batch.rows, vec![vec![Value::String("1".to_owned())]]);
    assert!(batch.done);
    cursor.complete(&connection).expect("complete");
    assert_eq!(
        branch_head(&connection, "interpolated").expect("new Branch"),
        branch_head(&connection, "main").expect("main")
    );
    let cursor = QueryCursor::new(prepared);
    assert!(
        cursor.has_version_operation(),
        "actual Version mutation must retain its lifecycle flag"
    );
    assert!(
        cursor.is_write(),
        "actual Branch creation must be classified as a write"
    );
}

#[test]
fn interpolation_does_not_bypass_version_composition_restrictions() {
    let connection = storage();
    for (query, options, kind) in [
        (
            "CREATE (:Audit) RETURN s\"{COUNT { CALL lithograph.branch.list() YIELD name RETURN name }}\"",
            "{}",
            QueryErrorKind::InvalidArgument,
        ),
        (
            "RETURN s\"{COUNT { CALL lithograph.branch.list() YIELD name RETURN name }}\"",
            r#"{"graphView":{"requireAllLabels":["Tenant"]}}"#,
            QueryErrorKind::InvalidArgument,
        ),
        (
            "UNWIND [1] AS x CALL (x) { RETURN s\"{COUNT { CALL lithograph.index.rebuild('missing','branch/main') YIELD name RETURN name }}\" AS result } IN TRANSACTIONS RETURN result",
            "{}",
            QueryErrorKind::TransactionBoundaryRequired,
        ),
    ] {
        let error = prepare(
            &connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::parse_text(options).expect("options"),
        )
        .expect_err("existing Version composition boundary");
        assert_eq!(error.kind, kind, "{query}: {error}");
    }
}

#[test]
fn candidate_interpolation_preserves_maintenance_and_version_context_boundaries() {
    let connection = storage();
    for query in [
        "CREATE (:Base) FINISH",
        "CALL lithograph.branch.create('side')",
        "CREATE (:MainOnly) FINISH",
        "CALL lithograph.branch.checkout('side')",
        "CREATE (:SideOnly) FINISH",
        "CALL lithograph.branch.checkout('main')",
    ] {
        rows(&connection, query);
    }
    let session = rows(
        &connection,
        "CALL lithograph.merge.start('branch/side') YIELD session,revision,status RETURN session,revision,status",
    );
    assert_eq!(session[0][2], Value::String("ready".to_owned()));
    let Value::String(id) = &session[0][0] else {
        panic!("Session identity");
    };
    let Value::Integer(revision) = session[0][1] else {
        panic!("Session revision");
    };
    let options =
        ExecutionOptions::parse(&serde_json::json!({"mergeSession":{"id":id,"revision":revision}}))
            .expect("candidate options");
    for query in [
        "RETURN s\"{COUNT { LOAD CSV FROM 'file:///__phase18_classification__.csv' AS row RETURN row }}\"",
        "RETURN s\"{COUNT { CALL db.index.semantic.rebuild('missing','branch/main') YIELD name RETURN name }}\"",
        "RETURN s\"{COUNT { CALL lithograph.branch.list() YIELD name RETURN name }}\"",
    ] {
        let error = prepare(&connection, query, BTreeMap::new(), options.clone())
            .expect_err("candidate context cannot change lifecycle");
        assert_eq!(
            error.kind,
            QueryErrorKind::TransactionBoundaryRequired,
            "{query}: {error}"
        );
    }
    prepare(&connection, "RETURN s\"{COUNT { CALL db.index.semantic.queryNodes('missing','text',{limit:1}) YIELD node RETURN node }}\"", BTreeMap::new(), options).expect("ordinary Semantic graph query keeps its existing candidate capability");
}
