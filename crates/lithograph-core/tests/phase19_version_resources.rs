use std::cell::Cell;
use std::collections::BTreeMap;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, PreparedQuery, QueryCursor, QueryError, QueryErrorKind, QueryResources,
    prepare, prepare_with_resources_and_interrupt,
};
use lithograph_core::storage::{
    HashId, STORAGE_FORMAT, branch_head, create_storage_schema, initialize_connection_state,
    initialize_root, load_merge_session,
};
use rusqlite::Connection;

const SMALL_BUDGET: u64 = 16 * 1024 * 1024;
const LOW_BUDGET: u64 = 512 * 1024;
const HIGH_BUDGET: u64 = 256 * 1024 * 1024;
const LARGE_PAYLOAD: usize = 2 * 1024 * 1024;
const CANDIDATE_QUERY: &str = "MATCH (n:Item) RETURN n.left, n.right, size(n.payload)";

struct ReadyMerge {
    connection: Connection,
    session: String,
    revision: i64,
    ours: HashId,
    theirs: HashId,
}

fn database() -> Connection {
    let connection = Connection::open_in_memory().expect("in-memory SQLite");
    connection.execute_batch("CREATE TABLE main._lithograph_meta(id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);").expect("metadata table");
    connection.execute("INSERT INTO main._lithograph_meta VALUES(1,'lithograph-format-v1','00000000-0000-4000-8000-000000000019',?1)",
        [STORAGE_FORMAT]).expect("synthetic database identity");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root Commit");
    initialize_connection_state(&connection).expect("active Branch");
    connection
}

fn drain(connection: &Connection, prepared: PreparedQuery) -> Result<Vec<Vec<Value>>, QueryError> {
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 1)?;
        rows.extend(batch.rows.iter().cloned());
        let done = batch.done;
        drop(batch);
        if done {
            drop(cursor.complete(connection)?);
            return Ok(rows);
        }
    }
}

fn execute(
    connection: &Connection,
    text: &str,
    params: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Vec<Vec<Value>> {
    let prepared = prepare(connection, text, params, options).expect(text);
    drain(connection, prepared).expect(text)
}

fn call(connection: &Connection, text: &str) -> Vec<Vec<Value>> {
    execute(
        connection,
        text,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
}

fn ready_merge(payload_bytes: usize) -> ReadyMerge {
    let connection = database();
    call(
        &connection,
        "CREATE (:Item {left:0, right:0, payload:''}) FINISH",
    );
    call(
        &connection,
        "CALL lithograph.branch.create('feature', 'branch/main') YIELD name RETURN name",
    );
    call(&connection, "MATCH (n:Item) SET n.left=1 FINISH");
    let feature = ExecutionOptions::parse(&serde_json::json!({"branch":"feature"})).unwrap();
    execute(
        &connection,
        "MATCH (n:Item) SET n.right=2, n.payload=$payload FINISH",
        BTreeMap::from([(
            "payload".to_owned(),
            Value::String("x".repeat(payload_bytes)),
        )]),
        feature,
    );
    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/feature') YIELD session, revision, status, unresolved RETURN session, revision, status, unresolved",
    );
    assert_eq!(started[0][2], Value::String("ready".to_owned()));
    assert_eq!(started[0][3], Value::Integer(0));
    let Value::String(session) = &started[0][0] else {
        panic!("session id");
    };
    let Value::Integer(revision) = started[0][1] else {
        panic!("session revision");
    };
    ReadyMerge {
        ours: branch_head(&connection, "main").unwrap(),
        theirs: branch_head(&connection, "feature").unwrap(),
        connection,
        session: session.clone(),
        revision,
    }
}

fn candidate_options(fixture: &ReadyMerge, bytes: u64) -> ExecutionOptions {
    ExecutionOptions::parse(&serde_json::json!({"memoryLimitBytes":bytes,
        "mergeSession":{"id":fixture.session,"revision":fixture.revision}}))
    .unwrap()
}

fn assert_session_unchanged(fixture: &ReadyMerge) {
    assert_eq!(
        branch_head(&fixture.connection, "main").unwrap(),
        fixture.ours
    );
    assert_eq!(
        branch_head(&fixture.connection, "feature").unwrap(),
        fixture.theirs
    );
    assert_eq!(
        load_merge_session(&fixture.connection, &fixture.session)
            .unwrap()
            .unwrap()
            .revision,
        fixture.revision
    );
    assert!(
        fixture.connection.is_autocommit(),
        "candidate read SAVEPOINT was released"
    );
}

#[test]
fn small_graph_version_calls_and_merge_candidate_accept_explicit_budget() {
    let fixture = ready_merge(0);
    for (query, expected) in [
        (
            "MATCH (n:Item) RETURN n.left, n.right",
            vec![vec![Value::Integer(1), Value::Integer(0)]],
        ),
        (
            "CALL lithograph.branch.list() YIELD name RETURN name ORDER BY name",
            vec![
                vec![Value::String("feature".to_owned())],
                vec![Value::String("main".to_owned())],
            ],
        ),
        (
            "CALL lithograph.commit.get('branch/main') YIELD commit RETURN commit",
            vec![vec![Value::String(format!(
                "commit/{}",
                fixture.ours.to_hex()
            ))]],
        ),
    ] {
        let resources = QueryResources::new(Some(SMALL_BUDGET));
        let options =
            ExecutionOptions::parse(&serde_json::json!({"memoryLimitBytes":SMALL_BUDGET})).unwrap();
        let prepared = prepare_with_resources_and_interrupt(
            &fixture.connection,
            query,
            BTreeMap::new(),
            options,
            resources.clone(),
            &|| false,
        )
        .expect("bounded graph/version prepare");
        assert_eq!(
            drain(&fixture.connection, prepared).unwrap(),
            expected,
            "{query}"
        );
        assert_eq!(
            resources.used(),
            0,
            "version result and cursor released: {query}"
        );
    }
    let resources = QueryResources::new(Some(SMALL_BUDGET));
    let prepared = prepare_with_resources_and_interrupt(
        &fixture.connection,
        CANDIDATE_QUERY,
        BTreeMap::new(),
        candidate_options(&fixture, SMALL_BUDGET),
        resources.clone(),
        &|| false,
    )
    .expect("bounded candidate prepare");
    assert_eq!(
        drain(&fixture.connection, prepared).unwrap(),
        vec![vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(0)
        ]]
    );
    assert_eq!(resources.used(), 0);
    assert_session_unchanged(&fixture);
}

#[test]
fn stored_candidate_property_rejects_low_prepare_budget_and_succeeds_when_raised() {
    let fixture = ready_merge(LARGE_PAYLOAD);
    let resources = QueryResources::new(Some(LOW_BUDGET));
    let error = prepare_with_resources_and_interrupt(
        &fixture.connection,
        CANDIDATE_QUERY,
        BTreeMap::new(),
        candidate_options(&fixture, LOW_BUDGET),
        resources.clone(),
        &|| false,
    )
    .expect_err("stored candidate delta exceeds budget before owned clone");
    assert_eq!(error.kind, QueryErrorKind::Resource);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_TOOBIG));
    assert!(resources.peak() <= LOW_BUDGET);
    assert_eq!(
        resources.used(),
        0,
        "failed candidate preparation released all claims"
    );
    assert_session_unchanged(&fixture);

    let resources = QueryResources::new(Some(HIGH_BUDGET));
    let prepared = prepare_with_resources_and_interrupt(
        &fixture.connection,
        CANDIDATE_QUERY,
        BTreeMap::new(),
        candidate_options(&fixture, HIGH_BUDGET),
        resources.clone(),
        &|| false,
    )
    .expect("raised candidate budget");
    assert!(
        resources.used() >= LARGE_PAYLOAD as u64,
        "candidate delta remains accounted after prepare"
    );
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor
            .next_batch(&fixture.connection, 1)
            .expect("candidate rows");
        rows.extend(batch.rows.iter().cloned());
        if batch.done {
            assert_eq!(
                batch
                    .summary
                    .as_ref()
                    .unwrap()
                    .merge_session
                    .as_ref()
                    .unwrap()
                    .id,
                fixture.session
            );
            assert!(
                resources.used() < LARGE_PAYLOAD as u64,
                "terminal batch released large candidate work"
            );
            drop(batch);
            break;
        }
    }
    assert_eq!(
        rows,
        vec![vec![
            Value::Integer(1),
            Value::Integer(2),
            Value::Integer(LARGE_PAYLOAD as i64)
        ]]
    );
    drop(cursor.complete(&fixture.connection).unwrap());
    drop(cursor);
    assert_eq!(
        resources.used(),
        0,
        "terminal candidate, batches, and cursor released"
    );
    assert_session_unchanged(&fixture);
}

#[test]
fn candidate_cancel_and_early_cursor_drop_release_prepared_delta() {
    let fixture = ready_merge(LARGE_PAYLOAD);
    for cancel in [true, false] {
        let resources = QueryResources::new(Some(HIGH_BUDGET));
        let prepared = prepare_with_resources_and_interrupt(
            &fixture.connection,
            CANDIDATE_QUERY,
            BTreeMap::new(),
            candidate_options(&fixture, HIGH_BUDGET),
            resources.clone(),
            &|| false,
        )
        .unwrap();
        assert!(resources.used() >= LARGE_PAYLOAD as u64);
        let mut cursor = QueryCursor::new(prepared);
        if cancel {
            cursor.cancel(&fixture.connection).unwrap();
            assert_eq!(
                resources.used(),
                0,
                "cancel released candidate while cursor still exists"
            );
        }
        drop(cursor);
        assert_eq!(resources.used(), 0, "early cursor close released candidate");
        assert_session_unchanged(&fixture);
    }
}

#[test]
fn cancellation_during_candidate_resolution_releases_all_claims() {
    let fixture = ready_merge(LARGE_PAYLOAD);
    let resources = QueryResources::new(Some(HIGH_BUDGET));
    let canceled_inside_candidate = Cell::new(false);
    let interrupted = || {
        let cancel = resources.used() >= LARGE_PAYLOAD as u64;
        if cancel {
            canceled_inside_candidate.set(true);
        }
        cancel
    };
    let error = prepare_with_resources_and_interrupt(
        &fixture.connection,
        CANDIDATE_QUERY,
        BTreeMap::new(),
        candidate_options(&fixture, HIGH_BUDGET),
        resources.clone(),
        &interrupted,
    )
    .expect_err("cancel after candidate allocation was claimed");
    assert!(
        canceled_inside_candidate.get(),
        "cancellation occurred inside candidate work"
    );
    assert_eq!(error.kind, QueryErrorKind::Interrupted);
    assert_eq!(error.sqlite_code, Some(rusqlite::ffi::SQLITE_INTERRUPT));
    assert_eq!(
        resources.used(),
        0,
        "interrupted candidate released all claims"
    );
    assert_session_unchanged(&fixture);
    assert_eq!(
        call(&fixture.connection, "RETURN 1"),
        vec![vec![Value::Integer(1)]]
    );
}
