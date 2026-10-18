use std::cell::Cell;
use std::collections::BTreeMap;

use super::*;
use crate::query::{ExecutionOptions, QueryErrorKind, QueryResources, prepare};
use crate::storage::{branch_head, create_storage_schema, initialize_root};

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
             database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    let prepared = prepare(
        &connection,
        "CREATE (:ResourceProbe {value:3}), (:ResourceProbe {value:1}), (:ResourceProbe {value:2})",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare seed");
    let mut cursor = QueryCursor::new(prepared);
    let batch = cursor.next_batch(&connection, 1).expect("execute seed");
    assert!(batch.done, "seed has no result rows");
    cursor.complete(&connection).expect("complete seed");
    connection
}

fn options() -> ExecutionOptions {
    ExecutionOptions::parse(&serde_json::json!({"memoryLimitBytes": 16 * 1024 * 1024}))
        .expect("memory limit")
}

fn assert_ready_spill_interrupts(query: &str, distinct: bool) {
    let connection = fixture();
    let mut prepared = prepare(&connection, query, BTreeMap::new(), options()).expect(query);
    assert!(prepared.program.is_none(), "exercise the basic MATCH path");
    // Public DISTINCT currently lowers to ProgramStream. Select its equivalent
    // basic cursor barrier directly to exercise QueryCursor::cancel's state.
    prepared.distinct = distinct;
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let first = cursor.next_batch(&connection, 1).expect("first spill row");
    assert_eq!(first.rows.len(), 1);
    assert!(!first.done, "the ready spill still contains two rows");
    assert!(matches!(cursor.barrier, BarrierState::Ready(_)));
    assert!(cursor.spill_connection.is_some());
    drop(first);

    let error = cursor
        .next_batch_with_interrupt(&connection, 1, &|| true)
        .expect_err("interrupt while draining a ready spill");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    cursor
        .cancel(&connection)
        .expect("cancellation remains safe after interrupted cleanup");
    assert!(cursor.spill_connection.is_none());
    assert_eq!(resources.used(), 0, "interrupted spill released its leases");
    drop(cursor);
    assert_eq!(resources.used(), 0);
}

#[test]
fn ready_sort_spill_preserves_interrupt_and_allows_repeated_cleanup() {
    assert_ready_spill_interrupts(
        "MATCH (n:ResourceProbe) RETURN n.value AS value ORDER BY value",
        false,
    );
}

#[test]
fn ready_distinct_spill_preserves_interrupt_and_allows_repeated_cleanup() {
    assert_ready_spill_interrupts("MATCH (n:ResourceProbe) RETURN n.value AS value", true);
}

#[test]
fn sorted_output_batch_keeps_its_lease_after_cursor_cancellation_and_drop() {
    let connection = fixture();
    let prepared = prepare(
        &connection,
        "MATCH (n:ResourceProbe) RETURN $payload AS payload ORDER BY n.value",
        BTreeMap::from([("payload".to_owned(), Value::String("x".repeat(32 * 1024)))]),
        options(),
    )
    .expect("prepare sorted payload");
    assert!(prepared.program.is_none(), "exercise the basic MATCH path");
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let batch = cursor
        .next_batch(&connection, 1)
        .expect("first payload row");
    assert_eq!(batch.rows.len(), 1);
    assert!(!batch.done);
    let payload_bytes =
        super::super::resources::owned_row_bytes(&batch.rows[0]).expect("owned output size");
    cursor
        .cancel(&connection)
        .expect("close sorted cursor early");
    drop(cursor);
    assert!(
        resources.used() >= payload_bytes,
        "the caller's live output remains accounted after the cursor closes"
    );
    assert_eq!(batch.rows[0][0], Value::String("x".repeat(32 * 1024)));
    drop(batch);
    assert_eq!(resources.used(), 0, "dropping output returns its lease");
}

#[test]
fn order_alias_copy_checks_interrupt_inside_one_large_projected_value() {
    let connection = fixture();
    let mut prepared = prepare(
        &connection,
        "RETURN 1",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare alias fixture");
    prepared.columns = vec!["payload".to_owned()];
    prepared.order.clear();
    let snapshot = Snapshot::resolve(
        &connection,
        branch_head(&connection, "main").expect("current head"),
    )
    .expect("snapshot");
    let projected = [Value::String("x".repeat(256 * 1024))];
    let resources = QueryResources::default();
    let checks = Cell::new(0);
    let interrupted = || {
        let next = checks.get() + 1;
        checks.set(next);
        // The two reservation checks alone must not trigger cancellation;
        // the third checkpoint requires copying the long alias in chunks.
        next >= 3
    };
    let control = ExecutionControl::new(&resources, &interrupted);
    let error = super::project::order_values(
        &prepared,
        &snapshot,
        &BindingRow::default(),
        &projected,
        &control,
    )
    .err()
    .expect("alias copying must check before finishing the projected String");
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(checks.get(), 3);
    assert_eq!(resources.used(), 0, "failed alias copy releases its lease");
}
