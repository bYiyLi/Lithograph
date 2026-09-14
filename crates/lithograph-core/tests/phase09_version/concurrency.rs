use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, mpsc};
use std::thread;
use std::time::Duration;

use tempfile::tempdir;

use super::*;

fn create_file_storage(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).expect("file SQLite");
    connection
        .execute_batch(
            "PRAGMA journal_mode=WAL;\
             PRAGMA busy_timeout=5000;\
             CREATE TABLE main._lithograph_meta(\
                 id INTEGER PRIMARY KEY CHECK(id=1),\
                 magic TEXT NOT NULL,\
                 database_id TEXT NOT NULL,\
                 storage_format INTEGER NOT NULL\
             );\
             INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format)\
             VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000099', 2);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    initialize_connection_state(&connection).expect("connection state");
    connection
}

fn open_file_storage(path: &std::path::Path) -> Connection {
    let connection = Connection::open(path).expect("second SQLite connection");
    connection
        .execute_batch("PRAGMA busy_timeout=5000")
        .expect("busy timeout");
    initialize_connection_state(&connection).expect("connection state");
    connection
}

#[test]
fn version_mutation_keeps_query_start_branch_head_pinned() {
    let directory = tempdir().expect("temp directory");
    let path = directory.path().join("version-pin.db");
    let connection = create_file_storage(&path);
    execute(
        &connection,
        "CREATE (:Bulk {v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("base state");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('patch-source', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Bulk) SET n.v=1 FINISH",
        options(r#"{"branch":"patch-source"}"#),
    )
    .expect("source change");
    let source = branch_head(&connection, "patch-source").expect("source head");
    let patch = call(
        &connection,
        &format!(
            "CALL lithograph.diff('{}', '{}') YIELD patch RETURN patch",
            descriptor(base),
            descriptor(source)
        ),
    )[0][0]
        .clone();
    let mut params = BTreeMap::new();
    params.insert("patch".to_owned(), patch);
    let prepared = prepare(
        &connection,
        "CALL lithograph.patch.apply($patch) YIELD commit RETURN commit",
        params,
        ExecutionOptions::default(),
    )
    .expect("prepare patch against base HEAD");

    let writer = open_file_storage(&path);
    execute(
        &writer,
        "CREATE (:Concurrent) FINISH",
        ExecutionOptions::default(),
    )
    .expect("concurrent main write");
    let moved_head = branch_head(&writer, "main").expect("moved main head");
    assert_ne!(moved_head, base);

    let mut cursor = QueryCursor::new(prepared);
    let error = cursor
        .next_batch(&connection, 64)
        .expect_err("prepared Version mutation must not silently repin moved Branch");
    assert_eq!(error.kind, QueryErrorKind::BranchHeadMoved);
    assert_eq!(
        branch_head(&connection, "main").expect("main stays at concurrent head"),
        moved_head
    );
}

#[test]
fn version_mutation_reports_deleted_pinned_target_branch() {
    let directory = tempdir().expect("temp directory");
    let path = directory.path().join("deleted-target.db");
    let connection = create_file_storage(&path);
    let main = branch_head(&connection, "main").expect("main head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('stale-target', '{}') YIELD name RETURN name",
            descriptor(main)
        ),
    );
    call(
        &connection,
        "CALL lithograph.branch.checkout('stale-target') YIELD name RETURN name",
    );
    let prepared = prepare(
        &connection,
        "CALL lithograph.reset('branch/main') YIELD to RETURN to",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare reset on stale target");

    let writer = open_file_storage(&path);
    call(
        &writer,
        "CALL lithograph.branch.delete('stale-target') YIELD name RETURN name",
    );

    let mut cursor = QueryCursor::new(prepared);
    let error = cursor
        .next_batch(&connection, 64)
        .expect_err("deleted pinned target Branch must not report success");
    assert_eq!(error.kind, QueryErrorKind::BranchNotFound);
}

#[test]
fn concurrent_resolve_reports_session_changed_instead_of_busy_snapshot() {
    let directory = tempdir().expect("temp directory");
    let path = directory.path().join("merge-concurrency.db");
    let connection = create_file_storage(&path);
    execute(
        &connection,
        "UNWIND range(1, 200) AS i CREATE (:Bulk {i:i, v:0}) FINISH",
        ExecutionOptions::default(),
    )
    .expect("bulk base");
    let base = branch_head(&connection, "main").expect("base head");
    call(
        &connection,
        &format!(
            "CALL lithograph.branch.create('concurrent-resolve', '{}') YIELD name RETURN name",
            descriptor(base)
        ),
    );
    execute(
        &connection,
        "MATCH (n:Bulk) SET n.v=1 FINISH",
        ExecutionOptions::default(),
    )
    .expect("ours");
    execute(
        &connection,
        "MATCH (n:Bulk) SET n.v=2 FINISH",
        options(r#"{"branch":"concurrent-resolve"}"#),
    )
    .expect("theirs");
    let started = call(
        &connection,
        "CALL lithograph.merge.start('branch/concurrent-resolve') YIELD session RETURN session",
    );
    let session = string(&started[0][0]).to_owned();
    let (conflicts, _) = collect_conflict_pages(&connection, &session);
    let first = conflicts.first().expect("first conflict").0.clone();
    let last = conflicts.last().expect("last conflict").0.clone();

    let query = format!(
        "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{last}', choice:'ours'}}]) YIELD revision RETURN revision"
    );
    let prepared = prepare(
        &connection,
        &query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )
    .expect("prepare stale resolver");
    let mut cursor = QueryCursor::new(prepared);

    let (trigger_tx, trigger_rx) = mpsc::sync_channel::<()>(0);
    let (resume_tx, resume_rx) = mpsc::sync_channel::<()>(0);
    let callbacks = Arc::new(AtomicUsize::new(0));
    let callback_count = Arc::clone(&callbacks);
    connection
        .progress_handler(
            1,
            Some(move || {
                if callback_count.fetch_add(1, Ordering::Relaxed) == 100
                    && (trigger_tx.send(()).is_err()
                        || resume_rx.recv_timeout(Duration::from_secs(5)).is_err())
                {
                    return true;
                }
                false
            }),
        )
        .expect("install progress handler");

    let writer_path = path.clone();
    let writer_session = session.clone();
    let writer = thread::spawn(move || {
        trigger_rx
            .recv_timeout(Duration::from_secs(5))
            .expect("stale resolver reached read snapshot");
        let writer_connection = open_file_storage(&writer_path);
        let outcome = execute(
            &writer_connection,
            &format!(
                "CALL lithograph.merge.resolve('{writer_session}', 1, [{{conflictId:'{first}', choice:'ours'}}]) YIELD revision RETURN revision"
            ),
            ExecutionOptions::default(),
        );
        resume_tx.send(()).expect("resume stale resolver");
        outcome
    });

    let error = cursor
        .next_batch(&connection, 64)
        .expect_err("concurrent resolve must stale the first caller");
    connection
        .progress_handler(0, None::<fn() -> bool>)
        .expect("remove progress handler");
    let writer_rows = writer
        .join()
        .expect("writer thread")
        .expect("concurrent resolve succeeds")
        .0;
    assert_eq!(writer_rows[0][0], Value::Integer(2));
    assert!(callbacks.load(Ordering::Relaxed) > 100);
    assert_eq!(error.kind, QueryErrorKind::MergeSessionChanged);
}
