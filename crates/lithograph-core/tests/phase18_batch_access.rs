use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QueryError, QueryErrorKind, QuerySummary, prepare,
};
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
    ExecutionOptions::parse_text(text).expect("valid access options")
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), QueryError> {
    let prepared = prepare(connection, query, BTreeMap::new(), options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 1)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(connection)?));
        }
    }
}

fn read(connection: &Connection, query: &str) -> Vec<Vec<Value>> {
    execute(connection, query, ExecutionOptions::default())
        .expect("unrestricted verification query")
        .0
}

fn commit_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("Commit count")
}

fn write_policy() -> ExecutionOptions {
    options(r#"{"writeAccess":{"denyProperties":["secret"]}}"#)
}

fn read_policy() -> ExecutionOptions {
    options(r#"{"readAccess":{"query":{"denyProperties":["secret"]}}}"#)
}

#[test]
fn sequential_and_concurrent_batches_share_dynamic_write_rules_and_continue_status() {
    for boundary in [
        "TRANSACTIONS OF 1 ROWS",
        "2 CONCURRENT TRANSACTIONS OF 1 ROWS DISJOINT BY (value)",
    ] {
        let connection = fresh_storage();
        let before = commit_count(&connection);
        let query = format!(
            "UNWIND [1,2,3] AS value \
             CALL (value) {{ \
                 CREATE (n:Batched {{value:value}}) \
                 SET n[CASE value WHEN 2 THEN 'secret' ELSE 'open' END] = value \
             }} IN {boundary} ON ERROR CONTINUE REPORT STATUS AS status \
             RETURN value, status.started AS started, status.committed AS committed \
             ORDER BY value"
        );
        let (rows, summary) = execute(&connection, &query, write_policy())
            .expect("access refusal is handled by ON ERROR CONTINUE");
        assert_eq!(
            rows,
            vec![
                vec![
                    Value::Integer(1),
                    Value::Boolean(true),
                    Value::Boolean(true)
                ],
                vec![
                    Value::Integer(2),
                    Value::Boolean(true),
                    Value::Boolean(false)
                ],
                vec![
                    Value::Integer(3),
                    Value::Boolean(true),
                    Value::Boolean(true)
                ],
            ],
            "{boundary}"
        );
        assert_eq!(summary.counters.nodes_created, 2);
        assert_eq!(commit_count(&connection), before + 2, "{boundary}");
        assert_eq!(
            read(
                &connection,
                "MATCH (n:Batched) RETURN n.value, n.open, n.secret ORDER BY n.value"
            ),
            vec![
                vec![Value::Integer(1), Value::Integer(1), Value::Null],
                vec![Value::Integer(3), Value::Integer(3), Value::Null],
            ]
        );
    }
}

#[test]
fn rejected_batch_rolls_back_every_row_in_that_batch() {
    for boundary in [
        "TRANSACTIONS OF 2 ROWS",
        "2 CONCURRENT TRANSACTIONS OF 2 ROWS DISJOINT BY (value)",
    ] {
        let connection = fresh_storage();
        let before = commit_count(&connection);
        let query = format!(
            "UNWIND [1,2,3] AS value \
             CALL (value) {{ \
                 CREATE (n:AtomicBatch {{value:value}}) \
                 SET n[CASE value WHEN 2 THEN 'secret' ELSE 'open' END] = value \
             }} IN {boundary} ON ERROR CONTINUE REPORT STATUS AS status \
             RETURN value, status.committed AS committed ORDER BY value"
        );
        let (rows, _) = execute(&connection, &query, write_policy())
            .expect("later batch succeeds after failed two-row batch");
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Boolean(false)],
                vec![Value::Integer(2), Value::Boolean(false)],
                vec![Value::Integer(3), Value::Boolean(true)],
            ],
            "{boundary}"
        );
        assert_eq!(commit_count(&connection), before + 1, "{boundary}");
        assert_eq!(
            read(
                &connection,
                "MATCH (n:AtomicBatch) RETURN n.value, n.open, n.secret"
            ),
            vec![vec![Value::Integer(3), Value::Integer(3), Value::Null]],
            "{boundary}"
        );
    }
}

#[test]
fn fail_and_break_keep_prior_durable_batches_after_access_refusal() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let error = execute(
        &connection,
        "UNWIND [1,2,3] AS value \
         CALL (value) { \
             CREATE (n:FailedBatch {value:value}) \
             SET n[CASE value WHEN 2 THEN 'secret' ELSE 'open' END] = value \
         } IN TRANSACTIONS OF 1 ROWS FINISH",
        write_policy(),
    )
    .expect_err("default FAIL surfaces ACCESS_DENIED");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(commit_count(&connection), before + 1);
    assert_eq!(
        read(&connection, "MATCH (n:FailedBatch) RETURN n.value"),
        vec![vec![Value::Integer(1)]]
    );

    let before_break = commit_count(&connection);
    let (rows, _) = execute(
        &connection,
        "UNWIND [1,2,3] AS value \
         CALL (value) { \
             CREATE (n:BrokenBatch {value:value}) \
             SET n[CASE value WHEN 2 THEN 'secret' ELSE 'open' END] = value \
         } IN TRANSACTIONS OF 1 ROWS ON ERROR BREAK REPORT STATUS AS status \
         RETURN value, status.started AS started, status.committed AS committed \
         ORDER BY value",
        write_policy(),
    )
    .expect("BREAK preserves status rows for skipped inputs");
    assert_eq!(
        rows,
        vec![
            vec![
                Value::Integer(1),
                Value::Boolean(true),
                Value::Boolean(true)
            ],
            vec![
                Value::Integer(2),
                Value::Boolean(true),
                Value::Boolean(false)
            ],
            vec![
                Value::Integer(3),
                Value::Boolean(false),
                Value::Boolean(false)
            ],
        ]
    );
    assert_eq!(commit_count(&connection), before_break + 1);
    assert_eq!(
        read(&connection, "MATCH (n:BrokenBatch) RETURN n.value"),
        vec![vec![Value::Integer(1)]]
    );
}

#[test]
fn sequential_and_concurrent_read_batches_mask_properties_and_outer_conditions() {
    for boundary in [
        "TRANSACTIONS OF 1 ROWS",
        "2 CONCURRENT TRANSACTIONS OF 1 ROWS DISJOINT BY (n.open)",
    ] {
        let connection = fresh_storage();
        execute(
            &connection,
            "UNWIND [1,2,3] AS value CREATE (:Source {open:value, secret:value}) FINISH",
            ExecutionOptions::default(),
        )
        .expect("seed Source");
        let before = commit_count(&connection);
        let head = branch_head(&connection, "main").expect("head");
        let query = format!(
            "MATCH (n:Source) WHERE n.secret IS NULL \
             CALL (n) {{ \
                 RETURN n.open AS number, n.secret AS hidden, \
                        n['secret'] AS dynamicHidden, properties(n).secret AS mapHidden \
             }} IN {boundary} \
             WITH number, hidden, dynamicHidden, mapHidden \
             WHERE hidden IS NULL AND dynamicHidden IS NULL AND mapHidden IS NULL \
             RETURN number, hidden ORDER BY number"
        );
        let (rows, _) = execute(&connection, &query, read_policy())
            .expect("read rules are inherited before, inside, and after each batch");
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Integer(2), Value::Null],
                vec![Value::Integer(3), Value::Null],
            ],
            "{boundary}"
        );
        assert_eq!(commit_count(&connection), before);
        assert_eq!(branch_head(&connection, "main").expect("head"), head);
    }
}

#[test]
fn batch_write_rhs_uses_masked_values_while_canonical_sources_remain_intact() {
    for boundary in [
        "TRANSACTIONS OF 1 ROWS",
        "2 CONCURRENT TRANSACTIONS OF 1 ROWS DISJOINT BY (n.open)",
    ] {
        let connection = fresh_storage();
        execute(
            &connection,
            "UNWIND [1,2] AS value CREATE (:Source {open:value, secret:value}) FINISH",
            ExecutionOptions::default(),
        )
        .expect("seed Source");
        let before = commit_count(&connection);
        let query = format!(
            "MATCH (n:Source) \
             CALL (n) {{ \
                 CREATE (copy:Copy {{open:n.open, copiedSecret:n.secret}}) \
                 RETURN copy.open AS number, copy.copiedSecret AS hidden \
             }} IN {boundary} RETURN number, hidden ORDER BY number"
        );
        let (rows, _) = execute(&connection, &query, read_policy())
            .expect("mutating batches inherit query rules on RHS expressions");
        assert_eq!(
            rows,
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Integer(2), Value::Null],
            ]
        );
        assert_eq!(commit_count(&connection), before + 2);
        assert_eq!(
            read(
                &connection,
                "MATCH (n:Source) RETURN n.open, n.secret ORDER BY n.open"
            ),
            vec![
                vec![Value::Integer(1), Value::Integer(1)],
                vec![Value::Integer(2), Value::Integer(2)],
            ]
        );
        assert_eq!(
            read(
                &connection,
                "MATCH (n:Copy) RETURN n.open, n.copiedSecret ORDER BY n.open"
            ),
            vec![
                vec![Value::Integer(1), Value::Null],
                vec![Value::Integer(2), Value::Null],
            ]
        );
    }
}

#[test]
fn denied_outer_suffix_does_not_undo_completed_inner_batch() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let error = execute(
        &connection,
        "UNWIND [1] AS value \
         CALL (value) { CREATE (n:Inner {open:value}) RETURN n } \
         IN TRANSACTIONS OF 1 ROWS SET n.secret = 1 FINISH",
        write_policy(),
    )
    .expect_err("outer suffix must inherit write restriction");
    assert_eq!(error.kind, QueryErrorKind::AccessDenied);
    assert_eq!(commit_count(&connection), before + 1);
    assert_eq!(
        read(&connection, "MATCH (n:Inner) RETURN n.open, n.secret"),
        vec![vec![Value::Integer(1), Value::Null]]
    );
}

#[test]
fn cancellation_keeps_only_the_already_published_permitted_batch() {
    let connection = fresh_storage();
    let before = commit_count(&connection);
    let prepared = prepare(
        &connection,
        "UNWIND [1,2,3] AS value \
         CALL (value) { \
             CREATE (n:CancelledBatch {value:value}) \
             SET n[CASE value WHEN 2 THEN 'secret' ELSE 'open' END] = value \
         } IN 2 CONCURRENT TRANSACTIONS OF 1 ROWS DISJOINT BY (value) RETURN value",
        BTreeMap::new(),
        write_policy(),
    )
    .expect("prepare controlled concurrent batch stream");
    let mut cursor = QueryCursor::new(prepared);
    let first = cursor.next_batch(&connection, 1).expect("first batch");
    assert_eq!(first.rows, vec![vec![Value::Integer(1)]]);
    assert!(!first.done);
    assert_eq!(commit_count(&connection), before + 1);
    cursor
        .cancel(&connection)
        .expect("cancel remaining batches");
    assert_eq!(commit_count(&connection), before + 1);
    assert_eq!(
        read(
            &connection,
            "MATCH (n:CancelledBatch) RETURN n.value, n.secret"
        ),
        vec![vec![Value::Integer(1), Value::Null]]
    );
}
