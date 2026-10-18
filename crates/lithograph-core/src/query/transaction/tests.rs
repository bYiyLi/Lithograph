use super::*;
use crate::query::{ExecutionOptions, QueryCursor, prepare};

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata");
    storage::create_storage_schema(&connection).expect("storage schema");
    storage::initialize_root(&connection).expect("root");
    connection
}

#[test]
fn conditional_transaction_keeps_all_branch_result_reservations() {
    let connection = fixture();
    let prepared = prepare(
        &connection,
        "UNWIND [1,2,3] AS seed RETURN seed NEXT { WHEN true THEN { CALL (seed) { RETURN range(1,1024) AS vals } IN TRANSACTIONS OF 1 ROW RETURN vals } ELSE { RETURN [] AS vals } }",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("conditional transaction program");
    let program = prepared.program.as_ref().expect("prepared program");
    let resources = QueryResources::default();
    let mut metrics = QueryMetrics::default();
    let mut runtime = TransactionRuntime {
        connection: &connection,
        program,
        options: program
            .transaction_options
            .as_ref()
            .expect("transaction options"),
        params: &prepared.params,
        metrics: &mut metrics,
        resources: &resources,
        is_interrupted: &|| false,
        commit: prepared.commit,
        counters: QueryCounters::default(),
        next_batch_id: 1,
    };
    let result = execute_transaction_query(&mut runtime, &program.root, RowSet::seed())
        .expect("execute conditional transactions");
    assert_eq!(result.rows.len(), 3);
    let retained_bytes = result
        .rows
        .iter()
        .try_fold(0, |bytes, row| {
            super::super::resources::checked_add(bytes, expression::binding_row_bytes(row)?)
        })
        .expect("retained row bytes");
    assert!(
        result.memory.as_ref().expect("result reservation").bytes() >= retained_bytes,
        "every retained branch result must keep its memory reservation"
    );
    drop(result);
    assert_eq!(resources.used(), 0);
}

#[test]
fn conditional_transaction_results_cannot_outlive_their_budget() {
    let connection = fixture();
    let budget = 16 * 1024 * 1024;
    let options = ExecutionOptions::parse(&serde_json::json!({"memoryLimitBytes": budget}))
        .expect("budget options");
    let mut cursor = QueryCursor::new(
        prepare(
            &connection,
            "UNWIND range(1,128) AS seed RETURN seed NEXT { WHEN true THEN { CALL (seed) { RETURN range(1,5000) AS vals } IN TRANSACTIONS OF 1 ROW RETURN vals } ELSE { RETURN [] AS vals } }",
            BTreeMap::new(),
            options,
        )
        .expect("conditional transaction program"),
    );
    let resources = cursor.resources().clone();
    let error = cursor
        .next_batch(&connection, 1)
        .expect_err("all conditional branch results share the execution budget");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(resources.peak() <= budget);
    assert_eq!(resources.used(), 0);
    assert!(connection.is_autocommit());
}
