use std::cell::Cell;
use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, prepare};
use lithograph_core::storage::{
    self, CommitMetadata, HashId, LayerBuilder, OwnerKind, PropertyValue,
};
use rusqlite::Connection;

const MIB: u64 = 1024 * 1024;

fn run(
    db: &Connection,
    text: &str,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, QueryError> {
    let prepared = prepare(db, text, BTreeMap::new(), options)?;
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(db, 16)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(db)?;
            break;
        }
    }
    drop(cursor);
    assert_eq!(resources.used(), 0, "complete releases query leases");
    Ok(rows)
}

fn query(db: &Connection, text: &str) -> Vec<Vec<Value>> {
    run(db, text, ExecutionOptions::default()).expect(text)
}

fn fixture() -> (Connection, HashId) {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE TABLE _lithograph_meta(id INTEGER PRIMARY KEY, magic TEXT, database_id TEXT, storage_format INTEGER);").unwrap();
    storage::create_storage_schema(&db).unwrap();
    let root = storage::initialize_root(&db).unwrap().root;
    let label = storage::intern_label(&db, "Plain").unwrap();
    let mut layer = LayerBuilder::default();
    for value in 0..2 {
        let id = storage::allocate_node_id(&db).unwrap();
        layer.add_node(id).unwrap();
        layer.add_label(id, label).unwrap();
        for (name, value) in [("k", value), ("v", 1 - value)] {
            let key = storage::intern_property_key(&db, name).unwrap();
            layer
                .set_property(OwnerKind::Node, id, key, PropertyValue::Integer(value))
                .unwrap();
        }
        let payload = storage::intern_property_key(&db, "payload").unwrap();
        layer
            .set_property(
                OwnerKind::Node,
                id,
                payload,
                PropertyValue::String("x".repeat(2 * MIB as usize)),
            )
            .unwrap();
    }
    let commit =
        storage::commit_layer(&db, "main", root, None, &layer, &CommitMetadata::root()).unwrap();
    storage::create_checkpoint(&db, commit).unwrap();
    (db, commit)
}

fn limited() -> ExecutionOptions {
    ExecutionOptions::parse(&serde_json::json!({"memoryLimitBytes":MIB})).unwrap()
}

#[test]
fn plain_checkpoint_filter_is_lazy_for_unneeded_values_and_null_properties() {
    let (db, _) = fixture();
    assert_eq!(
        run(
            &db,
            "MATCH (n:Plain) WHERE n.v=1 RETURN n.k, n.absent",
            limited()
        )
        .unwrap(),
        vec![vec![Value::Integer(0), Value::Null]]
    );
    assert_eq!(
        query(
            &db,
            "MATCH (n:Plain) WHERE n.absent IS NULL RETURN count(n)"
        ),
        vec![vec![Value::Integer(2)]]
    );
    query(
        &db,
        "MATCH (a:Plain), (b:Plain) WHERE a.k=0 AND b.k=1 CREATE (a)-[:EDGE {v:7}]->(b) FINISH",
    );
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    assert_eq!(
        query(&db, "MATCH ()-[r:EDGE]->() RETURN r.v, r.absent"),
        vec![vec![Value::Integer(7), Value::Null]]
    );
}

#[test]
fn query_property_denial_precedes_large_value_decode() {
    let (db, _) = fixture();
    let options = ExecutionOptions::parse(&serde_json::json!({
        "memoryLimitBytes":MIB,"readAccess":{"query":{"denyProperties":["payload"]}}
    }))
    .unwrap();
    assert_eq!(
        run(
            &db,
            "MATCH (n:Plain) WHERE n.k=0 RETURN n.payload",
            options.clone()
        )
        .unwrap(),
        vec![vec![Value::Null]]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:Plain) WHERE n.payload IS NULL RETURN n.k ORDER BY n.k",
            options
        )
        .unwrap(),
        vec![vec![Value::Integer(0)], vec![Value::Integer(1)]]
    );
}

#[test]
fn deleted_owner_error_precedes_property_masking_and_rolls_back() {
    let (db, _) = fixture();
    let head = storage::branch_head(&db, "main").unwrap();
    let options = ExecutionOptions::parse(&serde_json::json!({
        "readAccess":{"query":{"denyProperties":["payload"]}}
    }))
    .unwrap();
    let error = run(
        &db,
        "MATCH (n:Plain) WHERE n.k=0 DETACH DELETE n RETURN n.payload",
        options,
    )
    .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Semantic);
    assert!(error.message.contains("no longer present"), "{error}");
    assert_eq!(storage::branch_head(&db, "main").unwrap(), head);
    assert_eq!(
        query(&db, "MATCH (n:Plain) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );
    assert!(db.is_autocommit());
}

#[test]
fn staged_dictionary_and_shadowed_checkpoint_values_keep_precedence() {
    let (db, old) = fixture();
    assert_eq!(
        run(
            &db,
            "MATCH (n:Plain) WHERE n.k=0 SET n.fresh=42 RETURN n.fresh",
            limited()
        )
        .unwrap(),
        vec![vec![Value::Integer(42)]]
    );
    // Seed the replacement through Storage: SET itself may need to inspect the
    // old value. This READ regression must not require that unrelated write to
    // decode a 2 MiB value inside its separate 1 MiB budget.
    let key = storage::find_property_key(&db, "payload").unwrap().unwrap();
    let mut layer = LayerBuilder::default();
    layer
        .set_property(OwnerKind::Node, 1, key, PropertyValue::Integer(7))
        .unwrap();
    storage::commit_layer(
        &db,
        "main",
        storage::branch_head(&db, "main").unwrap(),
        None,
        &layer,
        &CommitMetadata::root(),
    )
    .unwrap();
    assert_eq!(
        run(
            &db,
            "MATCH (n:Plain) WHERE n.k=0 RETURN n.fresh, n.payload",
            limited()
        )
        .unwrap(),
        vec![vec![Value::Integer(42), Value::Integer(7)]]
    );
    assert_eq!(
        run(
            &db,
            "MATCH (n:Plain) WHERE n.k=0 REMOVE n.payload RETURN n.payload",
            limited()
        )
        .unwrap(),
        vec![vec![Value::Null]]
    );
    let mut historical = limited();
    historical.snapshot = lithograph_core::query::SnapshotSelector::Commit(old.to_hex());
    assert_eq!(
        run(
            &db,
            "MATCH (n:Plain) WHERE n.k=0 RETURN n.fresh",
            historical
        )
        .unwrap(),
        vec![vec![Value::Null]]
    );
}

#[test]
fn named_statement_reprepares_after_host_ddl_and_rebinds_versioned_schema() {
    let (db, old) = fixture();
    let text = "MATCH (n:Plain) WHERE n.k=0 RETURN n.v, n.fresh";
    assert_eq!(query(&db, text), vec![vec![Value::Integer(1), Value::Null]]);
    db.execute_batch("CREATE TABLE host_touch(value INTEGER)")
        .unwrap();
    assert_eq!(query(&db, text), vec![vec![Value::Integer(1), Value::Null]]);
    query(
        &db,
        "CREATE CONSTRAINT typed FOR (n:Plain) REQUIRE n.v IS :: INTEGER",
    );
    query(&db, "CREATE RANGE INDEX plain_v FOR (n:Plain) ON (n.v)");
    assert_eq!(
        query(
            &db,
            "MATCH (n:Plain) WHERE n.k=0 SET n.fresh=8 RETURN n.fresh"
        ),
        vec![vec![Value::Integer(8)]]
    );
    storage::create_checkpoint(&db, storage::branch_head(&db, "main").unwrap()).unwrap();
    assert_eq!(
        query(&db, text),
        vec![vec![Value::Integer(1), Value::Integer(8)]]
    );
    query(&db, "DROP INDEX plain_v");
    query(&db, "DROP CONSTRAINT typed");
    assert_eq!(
        query(&db, text),
        vec![vec![Value::Integer(1), Value::Integer(8)]]
    );
    let mut historical = ExecutionOptions::default();
    historical.snapshot = lithograph_core::query::SnapshotSelector::Commit(old.to_hex());
    assert_eq!(
        run(&db, text, historical).unwrap(),
        vec![vec![Value::Integer(1), Value::Null]]
    );
}

#[test]
fn checkpoint_decode_resource_failure_releases_leases_and_can_retry() {
    let (db, _) = fixture();
    let prepared = prepare(
        &db,
        "MATCH (n:Plain) WHERE n.k=0 RETURN n.payload",
        BTreeMap::new(),
        limited(),
    )
    .unwrap();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let error = cursor.next_batch(&db, 1).unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert_eq!(resources.used(), 0);
    assert!(db.is_autocommit());
    drop(cursor);
    assert_eq!(
        query(&db, "MATCH (n:Plain) WHERE n.k=0 RETURN size(n.payload)"),
        vec![vec![Value::Integer(2 * MIB as i64)]]
    );
}

#[test]
fn checkpoint_decode_cancellation_and_early_close_release_leases() {
    let (db, _) = fixture();
    let prepared = prepare(
        &db,
        "MATCH (n:Plain) RETURN n.payload",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let canceled_in_value = Cell::new(false);
    let error = cursor
        .next_batch_with_interrupt(&db, 1, &|| {
            let canceled = resources.used() >= MIB;
            canceled_in_value.set(canceled_in_value.get() || canceled);
            canceled
        })
        .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert!(canceled_in_value.get());
    assert_eq!(resources.used(), 0);
    drop(cursor);
    let prepared = prepare(
        &db,
        "MATCH (n:Plain) RETURN n.k",
        BTreeMap::new(),
        limited(),
    )
    .unwrap();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    drop(cursor.next_batch(&db, 1).unwrap());
    cursor.cancel(&db).unwrap();
    assert_eq!(resources.used(), 0);
    drop(cursor);
    assert_eq!(
        query(&db, "MATCH (n:Plain) RETURN count(n)"),
        vec![vec![Value::Integer(2)]]
    );
}
