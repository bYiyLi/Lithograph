use std::collections::BTreeMap;

use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryErrorKind, prepare};
use lithograph_core::storage::{
    CommitMetadata, IndexConfiguration, IndexDefinition, IndexTarget, SchemaState,
    StandardIndexKind, branch_head, commit_schema, create_storage_schema, initialize_root,
};
use rusqlite::Connection;
use serde_json::json;

fn schema_fixture() -> Connection {
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
    let root = branch_head(&connection, "main").expect("root head");
    let definition = IndexDefinition {
        name: "positive".into(),
        kind: StandardIndexKind::Semantic,
        target: IndexTarget::NodeProperties {
            label: "Doc".into(),
            properties: vec!["text".into()],
        },
        owning_constraint: None,
        labels_or_types: vec!["Doc".into()],
        additional_properties: Vec::new(),
        configuration: Some(IndexConfiguration::Semantic {
            provider: "phase18-canonical-provider".into(),
            provider_config: json!({"nested": {"values": [0.0]}}),
            dimensions: 4,
            similarity_function: "cosine".into(),
        }),
    };
    let mut schema = SchemaState::default();
    schema.indexes.insert(definition.name.clone(), definition);
    let schema_hash = schema
        .persist(&connection)
        .expect("persist canonical Schema");
    commit_schema(
        &connection,
        "main",
        root,
        schema_hash,
        &CommitMetadata {
            author: None,
            message: None,
            committed_at: 18,
        },
    )
    .expect("install trusted historical definition without invoking a Provider");
    connection
}

fn create_semantic(name: &str, value: &str) -> String {
    format!(
        "CALL db.index.semantic.createNodeIndex('{name}',['Doc'],'text',\
         {{provider:'phase18-canonical-provider',providerConfig:{{nested:{{values:[{value}]}}}},\
         dimensions:4,similarity:'cosine'}})"
    )
}

#[test]
fn semantic_schema_equivalence_preserves_nested_provider_config_signed_zero() {
    let connection = schema_fixture();
    let head = branch_head(&connection, "main").expect("head");
    let prepared = prepare(
        &connection,
        &create_semantic("negative", "-0.0"),
        BTreeMap::new(),
        ExecutionOptions::default(),
    );
    assert!(
        prepared.is_ok(),
        "different canonical Provider configs must not be collapsed as equivalent: {:?}",
        prepared.err()
    );
    let error = prepare(
        &connection,
        &create_semantic("positive", "-0.0"),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("a different canonical config cannot replace the same Index name");
    assert_eq!(error.kind, QueryErrorKind::Schema);
    assert_eq!(error.message, "Index name positive is already in use");
    assert_eq!(branch_head(&connection, "main").expect("head"), head);
}

#[test]
fn identical_canonical_provider_config_still_rejects_an_equivalent_index() {
    let connection = schema_fixture();
    let head = branch_head(&connection, "main").expect("head");
    let error = prepare(
        &connection,
        &create_semantic("duplicate", "0.0"),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("same source and canonical config remains an equivalent Index");
    assert_eq!(error.kind, QueryErrorKind::Schema);
    assert!(error.message.contains("equivalent Index"), "{error}");
    let error = prepare(
        &connection,
        &create_semantic("positive", "0.0"),
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect_err("same Index name and canonical config is already present");
    assert_eq!(error.kind, QueryErrorKind::Schema);
    assert_eq!(error.message, "Index positive already exists");
    assert_eq!(branch_head(&connection, "main").expect("head"), head);

    let original_schema = SchemaState::load(&connection, head)
        .expect("original Schema")
        .canonical_blob()
        .expect("original canonical encoding");
    let prepared = prepare(
        &connection,
        "CREATE RANGE INDEX positive IF NOT EXISTS FOR (n:Other) ON (n.value)",
        BTreeMap::new(),
        ExecutionOptions::parse_text(r#"{"readAccess":{"allowExternalIO":false}}"#)
            .expect("no external I/O"),
    )
    .expect("IF NOT EXISTS retains the existing definition for an occupied Index name");
    let mut cursor = QueryCursor::new(prepared);
    while !cursor
        .next_batch(&connection, 16)
        .expect("execute IF NOT EXISTS without invoking the unavailable Provider")
        .done
    {}
    cursor.complete(&connection).expect("complete no-op DDL");
    let after = branch_head(&connection, "main").expect("head after IF NOT EXISTS");
    assert_ne!(
        after, head,
        "successful Schema DDL records its write intent"
    );
    assert_eq!(
        SchemaState::load(&connection, after)
            .expect("preserved Schema")
            .canonical_blob()
            .expect("preserved canonical encoding"),
        original_schema
    );
}
