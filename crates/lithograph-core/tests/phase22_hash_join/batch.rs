use super::*;
use std::collections::BTreeSet;

const BATCH_LIMIT: u64 = 256 * 1024;
const WIDE_QUERY: &str = "MATCH(n:Batch) RETURN n.seq AS seq,n.payload AS payload,\
    10/n.divisor AS checked";

fn plain_fixture() -> Connection {
    let db = empty_db();
    write(
        &db,
        "UNWIND range(0,599) AS seq CREATE(:Batch {seq:seq}) FINISH",
    );
    checkpoint(&db);
    db
}

fn pull_sequences(
    db: &Connection,
    cursor: &mut QueryCursor,
    requested: usize,
    count: usize,
    done: bool,
) -> Vec<i64> {
    let batch = cursor.next_batch(db, requested).unwrap();
    assert_eq!(batch.rows.len(), count);
    assert_eq!(batch.done, done);
    assert_eq!(batch.summary.is_some(), done);
    batch
        .rows
        .iter()
        .map(|row| match row.as_slice() {
            [Value::Integer(seq)] => *seq,
            _ => panic!("sequence projection: {row:?}"),
        })
        .collect()
}

fn finish_cursor(db: &Connection, mut cursor: QueryCursor, count: u64) {
    let resources = cursor.resources().clone();
    let summary = cursor.complete(db).unwrap();
    assert_eq!(summary.metrics.rows, count);
    drop(summary);
    let after = cursor.next_batch(db, 256).unwrap();
    assert!(after.done);
    assert!(after.rows.is_empty());
    assert!(after.summary.is_none());
    drop(after);
    drop(cursor);
    assert_eq!(resources.used(), 0);
}

#[test]
fn no_quota_direct_match_preserves_first_one_and_full_followup_batches() {
    let db = plain_fixture();
    let _scope = ReadScope::new(&db);
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = None;
    let prepared = prepare(&db, "MATCH(n:Batch) RETURN n.seq", BTreeMap::new(), options).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = pull_sequences(&db, &mut cursor, 1, 1, false);
    rows.extend(pull_sequences(&db, &mut cursor, 256, 256, false));
    rows.extend(pull_sequences(&db, &mut cursor, 256, 256, false));
    rows.extend(pull_sequences(&db, &mut cursor, 256, 87, true));
    rows.sort_unstable();
    assert_eq!(rows, (0..600).collect::<Vec<_>>());
    finish_cursor(&db, cursor, 600);
}

#[test]
fn direct_match_limit_reaches_its_terminal_batch_and_stays_done() {
    let db = plain_fixture();
    let _scope = ReadScope::new(&db);
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = None;
    let prepared = prepare(
        &db,
        "MATCH(n:Batch) RETURN n.seq LIMIT 259",
        BTreeMap::new(),
        options,
    )
    .unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = pull_sequences(&db, &mut cursor, 1, 1, false);
    rows.extend(pull_sequences(&db, &mut cursor, 256, 256, false));
    rows.extend(pull_sequences(&db, &mut cursor, 256, 2, true));
    let unique = rows.into_iter().collect::<BTreeSet<_>>();
    assert_eq!(unique.len(), 259);
    assert!(unique.iter().all(|seq| (0..600).contains(seq)));
    finish_cursor(&db, cursor, 259);
}

fn wide_fixture() -> Connection {
    let db = empty_db();
    consume(
        &db,
        "UNWIND range(0,7) AS seq CREATE(:Batch {seq:seq,payload:$payload,\
         divisor:CASE WHEN seq=7 THEN 0 ELSE 1 END}) FINISH",
        BTreeMap::from([("payload".into(), Value::String("x".repeat(64 * 1024)))]),
        ExecutionOptions::default(),
    )
    .unwrap();
    checkpoint(&db);
    db
}

fn partial_prefix(db: &Connection) -> QueryCursor {
    let mut options = ExecutionOptions::default();
    options.memory_limit_bytes = Some(BATCH_LIMIT);
    let prepared = prepare(db, WIDE_QUERY, BTreeMap::new(), options).unwrap();
    let mut cursor = QueryCursor::new(prepared);
    let first = cursor.next_batch(db, 1).unwrap();
    assert_eq!(first.rows.len(), 1);
    assert!(!first.done);
    assert!(first.summary.is_none());
    drop(first);
    let partial = cursor.next_batch(db, 256).unwrap();
    assert!(!partial.rows.is_empty());
    assert!(partial.rows.len() < 256);
    assert!(!partial.done);
    assert!(partial.summary.is_none());
    assert!(cursor.resources().peak() <= BATCH_LIMIT);
    drop(partial);
    cursor
}

#[test]
fn quota_partial_batch_keeps_the_later_projection_error() {
    let db = wide_fixture();
    let _scope = ReadScope::new(&db);
    let oracle = consume(
        &db,
        WIDE_QUERY,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .unwrap_err();
    // Existing integer arithmetic reports Type / division by zero, rather than
    // a NULL result or an error invented by the batch resource boundary.
    assert_eq!(oracle.kind, QueryErrorKind::Type);
    assert_eq!(oracle.message, "division by zero");
    let mut cursor = partial_prefix(&db);
    let resources = cursor.resources().clone();
    let error = loop {
        match cursor.next_batch(&db, 256) {
            Ok(batch) => {
                assert!(!batch.done, "the final bad divisor must still be evaluated");
                assert!(!batch.rows.is_empty());
            }
            Err(error) => break error,
        }
    };
    assert_eq!(error, oracle);
    drop(cursor);
    assert!(resources.peak() <= BATCH_LIMIT);
    assert_eq!(resources.used(), 0);
}

#[test]
fn quota_partial_batch_keeps_later_cancellation_and_releases_its_state() {
    let db = wide_fixture();
    let _scope = ReadScope::new(&db);
    let mut cursor = partial_prefix(&db);
    let resources = cursor.resources().clone();
    let error = cursor
        .next_batch_with_interrupt(&db, 256, &|| true)
        .unwrap_err();
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    drop(cursor);
    assert!(resources.peak() <= BATCH_LIMIT);
    assert_eq!(resources.used(), 0);
    assert_eq!(
        db.query_row("SELECT 1", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        1
    );
}
