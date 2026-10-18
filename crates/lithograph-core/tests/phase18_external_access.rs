use std::collections::BTreeMap;

use lithograph_core::cypher::{NodeValue, Value};
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, prepare};
use lithograph_core::storage::{branch_head, create_storage_schema, initialize_root};
use rusqlite::Connection;

fn storage() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL,\
             database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    connection
}

fn options(text: &str) -> ExecutionOptions {
    ExecutionOptions::parse_text(text).expect("execution options")
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, QueryError> {
    execute_with_params(connection, query, BTreeMap::new(), options)
}

fn execute_with_params(
    connection: &Connection,
    query: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let prepared = prepare(connection, query, params, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 16)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection)?;
            return Ok(rows);
        }
    }
}

fn no_external_io() -> ExecutionOptions {
    options(r#"{"readAccess":{"allowExternalIO":false}}"#)
}

#[test]
fn fulltext_schema_is_denied_before_host_tokenizer_lookup_or_construction() {
    let connection = storage();
    let head = branch_head(&connection, "main").expect("head");
    let ddl = "CREATE FULLTEXT INDEX text_index FOR (n:Doc) ON EACH [n.text] OPTIONS {indexConfig:{`fulltext.analyzer`:'phase18_not_registered'}}";
    for prefix in ["", "EXPLAIN ", "PROFILE "] {
        let error = execute(&connection, &format!("{prefix}{ddl}"), no_external_io())
            .expect_err("tokenizer authority denied before runtime lookup");
        assert_eq!(error.kind, QueryErrorKind::AccessDenied, "{error}");
        assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_AUTH));
        assert_eq!(branch_head(&connection, "main").expect("head"), head);
    }
    let error = execute(&connection, ddl, ExecutionOptions::default())
        .expect_err("default behavior reaches tokenizer lookup");
    assert_eq!(error.kind, QueryErrorKind::Schema);
    assert!(error.message.contains("FULLTEXT Index"));
}

#[test]
fn external_io_permission_does_not_override_schema_or_readonly_restrictions() {
    let connection = storage();
    let ddl = "CREATE FULLTEXT INDEX text_index FOR (n:Doc) ON EACH [n.text]";
    let error = execute(
        &connection,
        ddl,
        options(
            r#"{"readAccess":{"allowExternalIO":true},"writeAccess":{"allowSchemaChanges":false}}"#,
        ),
    )
    .expect_err("schema permission remains required");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    let error = execute(
        &connection,
        ddl,
        options(r#"{"readOnlyNoExternalIO":true,"readAccess":{"allowExternalIO":true}}"#),
    )
    .expect_err("old read-only boundary retains its error");
    assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
    assert!(error.message.contains("readOnlyNoExternalIO"));
}

#[test]
fn ordinary_writes_and_standard_indexes_work_with_existing_host_backed_definitions() {
    let connection = storage();
    execute(
        &connection,
        "CREATE FULLTEXT INDEX text_index FOR (n:Doc) ON EACH [n.text]",
        ExecutionOptions::default(),
    )
    .expect("trusted tokenizer definition");
    for query in [
        "CREATE (:Doc {text:'first', v:1}) FINISH",
        "MATCH (n:Doc) SET n.text='second', n.v=2 FINISH",
        "CREATE RANGE INDEX version_index FOR (n:Doc) ON (n.v)",
        "CALL lithograph.index.rebuild('version_index','branch/main') YIELD name RETURN name",
        "CREATE FULLTEXT INDEX text_index IF NOT EXISTS FOR (n:Doc) ON EACH [n.text]",
    ] {
        execute(&connection, query, no_external_io()).expect(query);
    }
    assert_eq!(
        execute(
            &connection,
            "MATCH (n:Doc) RETURN n.text, n.v",
            no_external_io(),
        )
        .expect("ordinary reads"),
        vec![vec![Value::String("second".to_owned()), Value::Integer(2)]],
    );
    for query in [
        "MATCH (n:Doc) DELETE n",
        "DROP INDEX text_index",
        "DROP INDEX version_index",
    ] {
        execute(&connection, query, no_external_io()).expect(query);
    }
}

#[test]
fn semantic_create_arguments_use_query_property_visibility() {
    let connection = storage();
    let query = "CALL db.index.semantic.createNodeIndex($source.secret,['Doc'],'text',{provider:'phase18_not_registered',providerConfig:{},dimensions:2,similarity:'cosine'})";
    let params = BTreeMap::from([(
        "source".to_owned(),
        Value::Node(NodeValue {
            element_id: "n:1".to_owned(),
            labels: vec!["Doc".to_owned()],
            properties: BTreeMap::from([(
                "secret".to_owned(),
                Value::String("private_index_name".to_owned()),
            )]),
        }),
    )]);
    let error = execute_with_params(
        &connection,
        query,
        params,
        options(r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#),
    )
    .expect_err("hidden property yields null instead of an index name");
    assert_eq!(error.kind, QueryErrorKind::Schema, "{error}");
    assert!(error.message.contains("indexName"), "{error}");
    assert!(!error.message.contains("private_index_name"));
}
