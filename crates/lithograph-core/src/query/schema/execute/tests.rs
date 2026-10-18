use std::cell::Cell;

use super::*;
use crate::cypher::ClauseKind;
use crate::query::QueryErrorKind;
use crate::storage::{IndexDefinition, IndexTarget, StandardIndexKind};

fn fixture() -> (Connection, HashId, PreparedSchema) {
    let connection = Connection::open_in_memory().unwrap();
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL)").unwrap();
    storage::create_storage_schema(&connection).unwrap();
    let root = storage::initialize_root(&connection).unwrap().root;
    let name = "index_".to_owned() + &"wide".repeat(2_048);
    let mut state = SchemaState::default();
    state.indexes.insert(
        name.clone(),
        IndexDefinition {
            name,
            kind: StandardIndexKind::Range,
            target: IndexTarget::NodeProperties {
                label: "Person".to_owned(),
                properties: vec!["score".to_owned()],
            },
            owning_constraint: None,
            labels_or_types: Vec::new(),
            additional_properties: Vec::new(),
            configuration: None,
        },
    );
    let prepared = PreparedSchema {
        branch: "main".to_owned(),
        author: None,
        message: None,
        state,
        counters: SchemaCounters {
            indexes_added: 1,
            ..SchemaCounters::default()
        },
        kind: ClauseKind::CreateIndex,
    };
    (connection, root, prepared)
}

#[test]
fn ddl_execution_preflights_canonical_work_and_releases_failed_budget() {
    let (connection, root, prepared) = fixture();
    let resources = QueryResources::new(Some(32 * 1024));
    let error = execute_schema(&connection, &prepared, root, &|| false, &resources)
        .expect_err("canonical Schema work exceeds the execution budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_eq!(storage::branch_head(&connection, "main").unwrap(), root);
    assert_eq!(resources.used(), 0);
    assert!(resources.peak() <= 32 * 1024);

    let resources = QueryResources::new(Some(16 * 1024 * 1024));
    let outcome = execute_schema(&connection, &prepared, root, &|| false, &resources)
        .expect("the same DDL succeeds with sufficient budget");
    assert_ne!(outcome.commit, root);
    assert_eq!(
        storage::branch_head(&connection, "main").unwrap(),
        outcome.commit
    );
    assert_eq!(resources.used(), 0);
}

#[test]
fn ddl_execution_cancellation_observes_callback_before_publication() {
    let (connection, root, prepared) = fixture();
    let resources = QueryResources::new(Some(16 * 1024 * 1024));
    let checks = Cell::new(0);
    let interrupt = || {
        checks.set(checks.get() + 1);
        checks.get() >= 4
    };
    let error = execute_schema(&connection, &prepared, root, &interrupt, &resources)
        .expect_err("cancel Schema execution");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(storage::branch_head(&connection, "main").unwrap(), root);
    assert_eq!(resources.used(), 0);
    assert_eq!(checks.get(), 4);
}
