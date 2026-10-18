use lithograph_core::storage::{
    CommitMetadata, IndexConfiguration, IndexDefinition, IndexTarget, SchemaState,
    StandardIndexKind, commit_schema,
};

use super::*;

const DENY_SCHEMA: &str = r#"{"writeAccess":{"allowSchemaChanges":false}}"#;

fn schema_storage() -> Connection {
    let connection = fresh_storage();
    call(&connection, "CREATE (:Item {title:0}) FINISH");
    connection
}

// Fixture construction uses the public canonical storage API; all tested version
// operations below run through the real query engine without a Provider stub.
fn install_index(connection: &Connection, config: &str, branch: &str) -> HashId {
    let head = branch_head(connection, branch).expect("base head");
    let mut schema = SchemaState::load(connection, head).expect("canonical Schema");
    schema.indexes.insert(
        "canonical_sem".into(),
        IndexDefinition {
            name: "canonical_sem".into(),
            kind: StandardIndexKind::Semantic,
            target: IndexTarget::NodeProperties {
                label: "Item".into(),
                properties: vec!["text".into()],
            },
            owning_constraint: None,
            labels_or_types: vec!["Item".into()],
            additional_properties: vec![],
            configuration: Some(IndexConfiguration::Semantic {
                provider: "phase18-schema-canonical".into(),
                provider_config: serde_json::from_str(config).expect("fixture JSON"),
                dimensions: 2,
                similarity_function: "cosine".into(),
            }),
        },
    );
    let schema_hash = schema.persist(connection).expect("persist fixture Schema");
    commit_schema(
        connection,
        branch,
        head,
        schema_hash,
        &CommitMetadata {
            author: None,
            message: Some("canonical Schema fixture".into()),
            committed_at: commit_count(connection),
        },
    )
    .expect("publish authentic canonical Schema fixture")
}

fn schema_bytes(connection: &Connection, commit: HashId) -> Vec<u8> {
    SchemaState::load(connection, commit)
        .expect("canonical Schema")
        .canonical_blob()
        .expect("frozen Schema encoding")
}

fn counts(connection: &Connection) -> (i64, i64, i64, i64, i64) {
    connection
        .query_row(
            "SELECT (SELECT count(*) FROM main._lithograph_commits), \
                (SELECT count(*) FROM main._lithograph_layers), \
                (SELECT count(*) FROM main._lithograph_schema_objects), \
                (SELECT count(*) FROM main._lithograph_merge_sessions), \
                (SELECT count(*) FROM main._lithograph_merge_resolutions)",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )
        .expect("canonical counts")
}

fn patch_operations(patch: &Value) -> &[Value] {
    let Value::Map(patch) = patch else {
        panic!("Patch map")
    };
    let Value::List(operations) = &patch["operations"] else {
        panic!("operations list")
    };
    operations
}

fn schema_pairs() -> [(&'static str, &'static str); 2] {
    [
        (r#"{"t":0.0}"#, r#"{"t":-0.0}"#),
        (r#"{"nested":[{"t":0.0}]}"#, r#"{"nested":[{"t":-0.0}]}"#),
    ]
}

#[test]
fn semantic_schema_diff_retains_scalar_and_nested_signed_zero_identity() {
    for (positive, negative) in schema_pairs() {
        let connection = schema_storage();
        let before = install_index(&connection, positive, "main");
        let after = install_index(&connection, negative, "main");
        let expected = schema_bytes(&connection, after);
        assert_ne!(schema_bytes(&connection, before), expected);
        for (from, to) in [(before, after), (after, before)] {
            let patch = super::patch::diff(&connection, from, to);
            let operations = patch_operations(&patch);
            assert_eq!(operations.len(), 1, "one changed Index logical slot");
            let Value::Map(operation) = &operations[0] else {
                panic!("Index operation")
            };
            assert_eq!(operation["op"], Value::String("SetIndex".into()));
            assert_ne!(
                lithograph_core::cypher::encode_json_text(&operation["before"]),
                lithograph_core::cypher::encode_json_text(&operation["after"]),
                "the complete Index before/after retain canonical configuration",
            );
        }
    }
}

#[test]
fn semantic_schema_patch_before_condition_rejects_numerically_equal_wrong_definition() {
    let connection = schema_storage();
    let positive = install_index(&connection, r#"{"nested":[{"t":0.0}]}"#, "main");
    call(&connection, "DROP INDEX canonical_sem");
    let dropped = branch_head(&connection, "main").expect("dropped head");
    let drop_patch = super::patch::diff(&connection, positive, dropped);
    let negative = install_index(&connection, r#"{"nested":[{"t":-0.0}]}"#, "main");
    let set_patch = super::patch::diff(&connection, positive, negative);
    for patch in [drop_patch, set_patch] {
        let before = counts(&connection);
        let error = super::patch::apply(&connection, patch, "{}")
            .expect_err("before state must distinguish signed zero");
        assert_eq!(error.kind, QueryErrorKind::InvalidArgument);
        assert!(error.message.contains("before-condition"), "{error}");
        assert_eq!(branch_head(&connection, "main").expect("head"), negative);
        assert_eq!(counts(&connection), before);
    }
}

#[test]
fn reset_and_patch_check_opaque_schema_changes_without_mutating_history_on_denial() {
    let connection = schema_storage();
    let positive = install_index(&connection, r#"{"t":0.0}"#, "main");
    let negative = install_index(&connection, r#"{"t":-0.0}"#, "main");
    let inverse = super::patch::diff(&connection, negative, positive);
    let reset_query = format!(
        "CALL lithograph.reset('{}') YIELD to RETURN to",
        descriptor(positive)
    );
    let before = counts(&connection);
    assert_denied_unchanged(&connection, &reset_query, DENY_SCHEMA, "main");
    let error = super::patch::apply(&connection, inverse, DENY_SCHEMA)
        .expect_err("Schema-only Patch requires Schema permission");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(counts(&connection), before);
    assert_eq!(branch_head(&connection, "main").expect("head"), negative);
    call(&connection, "MATCH (n:Item) SET n.title=1 FINISH");
    assert_ne!(
        branch_head(&connection, "main").expect("graph change"),
        negative
    );
    execute(
        &connection,
        &format!(
            "CALL lithograph.reset('{}') YIELD to RETURN to",
            descriptor(negative)
        ),
        DENY_SCHEMA,
    )
    .expect("unchanged canonical Schema permits a ref-only Reset");
    assert_eq!(branch_head(&connection, "main").expect("head"), negative);
    assert_eq!(
        call(&connection, "MATCH (n:Item) RETURN n.title"),
        vec![vec![Value::Integer(0)]]
    );
    execute(&connection, &reset_query, "{}").expect("authorized Schema transition");
    assert_eq!(branch_head(&connection, "main").expect("head"), positive);
}

#[test]
fn ready_merge_finalize_rechecks_opaque_schema_identity_and_preserves_session_on_denial() {
    let connection = schema_storage();
    let base = install_index(&connection, r#"{"t":0.0}"#, "main");
    branch(&connection, "source", base);
    call(&connection, "MATCH (n:Item) SET n.title=1 FINISH");
    let target = branch_head(&connection, "main").expect("target head");
    let source = install_index(&connection, r#"{"t":-0.0}"#, "source");
    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/source') YIELD session, revision, status RETURN session, revision, status",
    );
    assert_eq!(started[0][2], Value::String("ready".into()));
    let session = string(&started[0][0]);
    let revision = integer(&started[0][1]);
    let query = format!(
        "CALL lithograph.merge.finalize('{session}',{revision}) YIELD status RETURN status"
    );
    let inspect = format!(
        "CALL lithograph.merge.get('{session}') YIELD revision, status RETURN revision, status"
    );
    let session_before = call(&connection, &inspect);
    let before = counts(&connection);
    assert_denied_unchanged(&connection, &query, DENY_SCHEMA, "main");
    assert_eq!(counts(&connection), before);
    assert_eq!(call(&connection, &inspect), session_before);
    assert_eq!(branch_head(&connection, "main").expect("target"), target);
    assert_ne!(
        schema_bytes(&connection, target),
        schema_bytes(&connection, source)
    );
    call(
        &connection,
        &format!(
            "CALL lithograph.merge.abort('{session}',{revision}) YIELD session RETURN session"
        ),
    );
    assert_eq!(counts(&connection).3, 0);
}
