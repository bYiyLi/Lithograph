use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use lithograph_core::query::{ExecutionOptions, QueryCursor, QueryError, prepare};
use lithograph_core::storage::{
    CommitMetadata, HashId, LayerBuilder, allocate_node_id, branch_head, capture_allocation_state,
    clear_commit_data, commit_data, commit_exists, commit_layer, create_branch_ref,
    create_checkpoint, create_merge_session, create_storage_schema, create_tag, delete_tag,
    initialize_connection_state, initialize_root, integrity_check, list_tags,
    load_merge_resolutions, load_merge_session, move_tag, set_commit_data,
    update_merge_resolutions,
};
use rusqlite::Connection;

fn database_path(label: &str) -> PathBuf {
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("system clock")
        .as_nanos();
    std::env::temp_dir().join(format!(
        "lithograph-phase10-{label}-{}-{nonce}.sqlite",
        std::process::id()
    ))
}

fn initialize_file_database(path: &PathBuf) -> Connection {
    let connection = Connection::open(path).expect("open recovery database");
    connection
        .execute_batch(
            "CREATE TABLE main._lithograph_meta(\
                 id INTEGER PRIMARY KEY CHECK(id=1),\
                 magic TEXT NOT NULL,\
                 database_id TEXT NOT NULL,\
                 storage_format INTEGER NOT NULL\
             );\
             INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format)\
             VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000011', 2);",
        )
        .expect("metadata");
    create_storage_schema(&connection).expect("storage schema");
    initialize_root(&connection).expect("root");
    initialize_connection_state(&connection).expect("connection state");
    connection
}

fn staged_commit(
    connection: &Connection,
    parent: lithograph_core::storage::HashId,
) -> lithograph_core::storage::HashId {
    let node = allocate_node_id(connection).expect("NodeId");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("add Node");
    let candidate = commit_layer(
        connection,
        "main",
        parent,
        None,
        &layer,
        &CommitMetadata {
            author: Some("phase10-recovery".to_owned()),
            message: Some("recovery candidate".to_owned()),
            committed_at: 1,
        },
    )
    .expect("stage Commit and Branch move");
    set_commit_data(connection, candidate, r#"{"state":"staged"}"#).expect("stage Commit Data");
    create_tag(connection, "staged", candidate).expect("stage Tag");
    create_checkpoint(connection, candidate).expect("stage checkpoint");
    candidate
}

fn cleanup_database(path: &PathBuf) {
    fs::remove_file(path).expect("remove recovery database");
    let _ = fs::remove_file(path.with_extension("sqlite-wal"));
    let _ = fs::remove_file(path.with_extension("sqlite-shm"));
}

fn run_crash_writer(path: &PathBuf, mode: &str, expected_code: i32) {
    let status =
        Command::new(std::env::current_exe().expect("current phase10 recovery test binary"))
            .arg("--exact")
            .arg("crash_writer_subprocess")
            .arg("--nocapture")
            .env("LITHOGRAPH_PHASE10_CRASH_DB", path)
            .env("LITHOGRAPH_PHASE10_CRASH_MODE", mode)
            .status()
            .expect("run crash-writer subprocess");
    assert_eq!(status.code(), Some(expected_code));
}

fn execute_query(connection: &Connection, query: &str) -> Result<(), QueryError> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::new(),
        ExecutionOptions::default(),
    )?;
    let mut cursor = QueryCursor::new(prepared);
    loop {
        let batch = cursor.next_batch(connection, 64)?;
        if batch.done {
            cursor.complete(connection)?;
            return Ok(());
        }
    }
}

#[test]
fn branch_move_fault_rolls_back_canonical_commit_and_layer() {
    let path = database_path("branch-move-fault");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");
    let commit_rows_before: i64 = connection
        .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })
        .expect("pre-fault Commit count");
    let layer_rows_before: i64 = connection
        .query_row("SELECT count(*) FROM main._lithograph_layers", [], |row| {
            row.get(0)
        })
        .expect("pre-fault Layer count");
    let node_delta_rows_before: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_node_delta",
            [],
            |row| row.get(0),
        )
        .expect("pre-fault Node delta count");

    let node = allocate_node_id(&connection).expect("NodeId");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("add Node");
    connection
        .execute_batch(
            "CREATE TEMP TRIGGER phase10_fail_branch_move \
             BEFORE UPDATE OF commit_id ON main._lithograph_branches \
             BEGIN \
               SELECT RAISE(ABORT, 'phase10 branch move fault'); \
             END;",
        )
        .expect("install branch-move fault trigger");

    let error = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: Some("phase10-recovery".to_owned()),
            message: Some("must roll back".to_owned()),
            committed_at: 2,
        },
    )
    .expect_err("branch-move fault must reject Commit");
    assert!(error.to_string().contains("phase10 branch move fault"));
    connection
        .execute_batch("DROP TRIGGER temp.phase10_fail_branch_move")
        .expect("remove branch-move fault trigger");

    assert_eq!(branch_head(&connection, "main").expect("head"), root);
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("post-fault Commit count"),
        commit_rows_before
    );
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM main._lithograph_layers", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("post-fault Layer count"),
        layer_rows_before
    );
    assert_eq!(
        connection
            .query_row(
                "SELECT count(*) FROM main._lithograph_node_delta",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("post-fault Node delta count"),
        node_delta_rows_before
    );
    assert!(
        integrity_check(&connection)
            .expect("integrity after branch-move fault")
            .is_empty()
    );
    drop(connection);
    cleanup_database(&path);
}

#[test]
fn checkpoint_write_fault_leaves_no_partial_derived_state_and_can_rebuild() {
    let path = database_path("checkpoint-write-fault");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");
    let node = allocate_node_id(&connection).expect("checkpoint NodeId");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("checkpoint Node");
    let commit = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: Some("phase10-recovery".to_owned()),
            message: Some("checkpoint fault candidate".to_owned()),
            committed_at: 3,
        },
    )
    .expect("checkpoint candidate Commit");

    connection
        .execute_batch(
            "CREATE TEMP TRIGGER phase10_fail_checkpoint_node \
             BEFORE INSERT ON main._lithograph_cp_nodes \
             BEGIN \
               SELECT RAISE(ABORT, 'phase10 checkpoint node fault'); \
             END;",
        )
        .expect("install checkpoint fault trigger");
    let error = create_checkpoint(&connection, commit)
        .expect_err("checkpoint write fault must reject partial checkpoint");
    assert!(error.to_string().contains("phase10 checkpoint node fault"));
    connection
        .execute_batch("DROP TRIGGER temp.phase10_fail_checkpoint_node")
        .expect("remove checkpoint fault trigger");

    for table in [
        "_lithograph_checkpoints",
        "_lithograph_cp_nodes",
        "_lithograph_cp_labels",
        "_lithograph_cp_relationships",
        "_lithograph_cp_properties",
    ] {
        let count: i64 = connection
            .query_row(
                &format!("SELECT count(*) FROM main.{table} WHERE commit_id = ?1"),
                [commit.as_bytes().as_slice()],
                |row| row.get(0),
            )
            .unwrap_or_else(|error| panic!("read {table} after checkpoint fault: {error}"));
        assert_eq!(count, 0, "{table} must not keep partial checkpoint rows");
    }
    assert!(commit_exists(&connection, commit).expect("canonical Commit survives"));
    assert!(
        integrity_check(&connection)
            .expect("integrity after checkpoint fault")
            .is_empty()
    );

    create_checkpoint(&connection, commit).expect("checkpoint rebuild after fault");
    let checkpoint_rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_checkpoints WHERE commit_id = ?1",
            [commit.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("rebuilt checkpoint row");
    let checkpoint_node_rows: i64 = connection
        .query_row(
            "SELECT count(*) FROM main._lithograph_cp_nodes WHERE commit_id = ?1",
            [commit.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("rebuilt checkpoint Node rows");
    assert_eq!(checkpoint_rows, 1);
    assert_eq!(checkpoint_node_rows, 1);
    assert!(
        integrity_check(&connection)
            .expect("integrity after checkpoint rebuild")
            .is_empty()
    );
    drop(connection);
    cleanup_database(&path);
}

#[test]
fn merge_abort_fault_rolls_back_session_and_resolutions() {
    let path = database_path("merge-abort-fault");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");
    let session = create_merge_session(&connection, "main", root, root, Some(root), 3)
        .expect("create Merge Session");
    let conflict = HashId::from_bytes([0x7a; 32]);
    let resolutions = BTreeMap::from([(conflict, r#"{"choice":"ours"}"#.to_owned())]);
    let revision = update_merge_resolutions(
        &connection,
        &session.id,
        session.revision,
        &resolutions,
        true,
    )
    .expect("seed Merge Session resolution");
    assert_eq!(revision, 2);

    connection
        .execute_batch(
            "CREATE TEMP TRIGGER phase10_fail_resolution_delete \
             BEFORE DELETE ON main._lithograph_merge_resolutions \
             BEGIN \
               SELECT RAISE(ABORT, 'phase10 resolution delete fault'); \
             END;",
        )
        .expect("install resolution-delete fault trigger");
    let error = execute_query(
        &connection,
        &format!(
            "CALL lithograph.merge.abort('{}', {revision}) YIELD session RETURN session",
            session.id
        ),
    )
    .expect_err("Merge Session abort fault must fail");
    assert!(
        error
            .to_string()
            .contains("phase10 resolution delete fault")
    );
    connection
        .execute_batch("DROP TRIGGER temp.phase10_fail_resolution_delete")
        .expect("remove resolution-delete fault trigger");

    let restored = load_merge_session(&connection, &session.id)
        .expect("load Merge Session after fault")
        .expect("Merge Session must survive failed abort");
    assert_eq!(restored.revision, revision);
    assert_eq!(
        load_merge_resolutions(&connection, &session.id).expect("restored resolutions"),
        resolutions
    );
    assert!(
        integrity_check(&connection)
            .expect("integrity after Merge Session abort fault")
            .is_empty()
    );
    drop(connection);
    cleanup_database(&path);
}

#[test]
fn merge_finalize_fault_rolls_back_ref_move_and_preserves_session() {
    let path = database_path("merge-finalize-fault");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");
    create_branch_ref(&connection, "source", root).expect("source Branch");
    let node = allocate_node_id(&connection).expect("source NodeId");
    let mut layer = LayerBuilder::default();
    layer.add_node(node).expect("source Node");
    let source = commit_layer(
        &connection,
        "source",
        root,
        None,
        &layer,
        &CommitMetadata {
            author: Some("phase10-recovery".to_owned()),
            message: Some("fast-forward source".to_owned()),
            committed_at: 4,
        },
    )
    .expect("source Commit");
    let session = create_merge_session(&connection, "main", root, source, Some(root), 5)
        .expect("create fast-forward Merge Session");

    connection
        .execute_batch(
            "CREATE TEMP TRIGGER phase10_fail_session_delete \
             BEFORE DELETE ON main._lithograph_merge_sessions \
             BEGIN \
               SELECT RAISE(ABORT, 'phase10 session delete fault'); \
             END;",
        )
        .expect("install session-delete fault trigger");
    let error = execute_query(
        &connection,
        &format!(
            "CALL lithograph.merge.finalize('{}', {}) YIELD status RETURN status",
            session.id, session.revision
        ),
    )
    .expect_err("Merge Session finalize fault must fail");
    assert!(error.to_string().contains("phase10 session delete fault"));
    connection
        .execute_batch("DROP TRIGGER temp.phase10_fail_session_delete")
        .expect("remove session-delete fault trigger");

    assert_eq!(
        branch_head(&connection, "main").expect("target head after failed finalize"),
        root
    );
    assert_eq!(
        load_merge_session(&connection, &session.id)
            .expect("load Merge Session after failed finalize"),
        Some(session)
    );
    assert!(commit_exists(&connection, source).expect("source Commit survives"));
    assert!(
        integrity_check(&connection)
            .expect("integrity after Merge Session finalize fault")
            .is_empty()
    );
    drop(connection);
    cleanup_database(&path);
}

#[test]
fn crash_writer_subprocess() {
    let Some(path) = std::env::var_os("LITHOGRAPH_PHASE10_CRASH_DB").map(PathBuf::from) else {
        return;
    };
    let mode = std::env::var("LITHOGRAPH_PHASE10_CRASH_MODE").expect("crash mode");
    let connection = Connection::open(path).expect("child open recovery database");
    initialize_connection_state(&connection).expect("child connection state");
    let root = branch_head(&connection, "main").expect("child root head");
    connection
        .execute_batch("PRAGMA journal_mode=WAL; BEGIN IMMEDIATE")
        .expect("child begin WAL writer transaction");
    staged_commit(&connection, root);
    match mode.as_str() {
        "before-commit" => std::process::exit(86),
        "after-commit" => {
            connection.execute_batch("COMMIT").expect("child commit");
            std::process::exit(87);
        }
        other => panic!("unknown crash mode {other}"),
    }
}

#[test]
fn process_death_before_sqlite_commit_recovers_pre_transaction_state() {
    let path = database_path("process-crash-before-commit");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");
    let allocations = capture_allocation_state(&connection).expect("pre-crash allocation state");
    drop(connection);

    run_crash_writer(&path, "before-commit", 86);

    let reopened = Connection::open(&path).expect("reopen after process death");
    initialize_connection_state(&reopened).expect("reinitialize connection state");
    assert_eq!(
        branch_head(&reopened, "main").expect("recovered head"),
        root
    );
    assert!(list_tags(&reopened).expect("recovered Tags").is_empty());
    let non_root_commits: i64 = reopened
        .query_row(
            "SELECT count(*) FROM main._lithograph_commits WHERE parent1 IS NOT NULL",
            [],
            |row| row.get(0),
        )
        .expect("count recovered non-root Commits");
    assert_eq!(non_root_commits, 0);
    assert_eq!(
        capture_allocation_state(&reopened).expect("recovered allocation state"),
        allocations
    );
    assert!(
        integrity_check(&reopened)
            .expect("integrity after process-death rollback")
            .is_empty()
    );
    drop(reopened);
    cleanup_database(&path);
}

#[test]
fn process_death_after_sqlite_commit_preserves_canonical_state() {
    let path = database_path("process-crash-after-commit");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");
    drop(connection);

    run_crash_writer(&path, "after-commit", 87);

    let reopened = Connection::open(&path).expect("reopen after committed process death");
    initialize_connection_state(&reopened).expect("reinitialize connection state");
    let candidate = branch_head(&reopened, "main").expect("durable committed head");
    assert_ne!(candidate, root);
    assert!(commit_exists(&reopened, candidate).expect("durable Commit existence"));
    assert_eq!(
        commit_data(&reopened, candidate).expect("durable Commit Data"),
        Some(r#"{"state":"staged"}"#.to_owned())
    );
    let tags = list_tags(&reopened).expect("durable Tags");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "staged");
    assert_eq!(tags[0].commit, candidate);
    let checkpoint_rows: i64 = reopened
        .query_row(
            "SELECT count(*) FROM main._lithograph_checkpoints WHERE commit_id = ?1",
            [candidate.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("durable checkpoint rows");
    assert_eq!(checkpoint_rows, 1);
    assert!(
        integrity_check(&reopened)
            .expect("integrity after committed process death")
            .is_empty()
    );
    drop(reopened);
    cleanup_database(&path);
}

#[test]
fn reopen_rolls_back_uncommitted_commit_ref_sidecars_and_checkpoint() {
    let path = database_path("writer-rollback");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");

    connection
        .execute_batch("BEGIN IMMEDIATE")
        .expect("begin writer transaction");
    let candidate = staged_commit(&connection, root);

    assert_eq!(
        branch_head(&connection, "main").expect("staged head"),
        candidate
    );
    assert_eq!(
        commit_data(&connection, candidate).expect("staged Commit Data"),
        Some(r#"{"state":"staged"}"#.to_owned())
    );
    assert_eq!(list_tags(&connection).expect("staged Tags").len(), 1);

    drop(connection);

    let reopened = Connection::open(&path).expect("reopen recovery database");
    initialize_connection_state(&reopened).expect("reinitialize connection state");
    assert_eq!(branch_head(&reopened, "main").expect("reopened head"), root);
    assert!(!commit_exists(&reopened, candidate).expect("candidate existence"));
    let sidecar_rows: i64 = reopened
        .query_row(
            "SELECT count(*) FROM main._lithograph_commit_data WHERE commit_id = ?1",
            [candidate.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("reopened Commit Data rows");
    assert_eq!(sidecar_rows, 0);
    let checkpoint_rows: i64 = reopened
        .query_row(
            "SELECT count(*) FROM main._lithograph_checkpoints WHERE commit_id = ?1",
            [candidate.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("reopened checkpoint rows");
    assert_eq!(checkpoint_rows, 0);
    assert!(list_tags(&reopened).expect("reopened Tags").is_empty());
    assert!(
        integrity_check(&reopened)
            .expect("integrity after reopen")
            .is_empty()
    );
    drop(reopened);
    cleanup_database(&path);
}

#[test]
fn reopen_preserves_committed_commit_ref_sidecars_and_checkpoint() {
    let path = database_path("writer-commit");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .expect("begin writer transaction");
    let candidate = staged_commit(&connection, root);
    connection
        .execute_batch("COMMIT")
        .expect("commit writer transaction");
    drop(connection);

    let reopened = Connection::open(&path).expect("reopen recovery database");
    initialize_connection_state(&reopened).expect("reinitialize connection state");
    assert_eq!(
        branch_head(&reopened, "main").expect("reopened head"),
        candidate
    );
    assert!(commit_exists(&reopened, candidate).expect("candidate existence"));
    assert_eq!(
        commit_data(&reopened, candidate).expect("reopened Commit Data"),
        Some(r#"{"state":"staged"}"#.to_owned())
    );
    let tags = list_tags(&reopened).expect("reopened Tags");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "staged");
    assert_eq!(tags[0].commit, candidate);
    let checkpoint_rows: i64 = reopened
        .query_row(
            "SELECT count(*) FROM main._lithograph_checkpoints WHERE commit_id = ?1",
            [candidate.as_bytes().as_slice()],
            |row| row.get(0),
        )
        .expect("reopened checkpoint rows");
    assert_eq!(checkpoint_rows, 1);
    assert!(
        integrity_check(&reopened)
            .expect("integrity after committed reopen")
            .is_empty()
    );
    drop(reopened);
    cleanup_database(&path);
}

#[test]
fn reopen_rolls_back_uncommitted_commit_data_and_tag_mutations() {
    let path = database_path("sidecar-ref-rollback");
    let connection = initialize_file_database(&path);
    let root = branch_head(&connection, "main").expect("root head");
    connection
        .execute_batch("BEGIN IMMEDIATE")
        .expect("begin seed transaction");
    let candidate = staged_commit(&connection, root);
    connection.execute_batch("COMMIT").expect("commit seed");

    connection
        .execute_batch("BEGIN IMMEDIATE")
        .expect("begin sidecar/ref mutation transaction");
    clear_commit_data(&connection, candidate).expect("clear Commit Data");
    assert_eq!(
        move_tag(&connection, "staged", root).expect("move Tag"),
        Some(candidate)
    );
    create_tag(&connection, "ephemeral", root).expect("create ephemeral Tag");
    assert_eq!(
        delete_tag(&connection, "staged").expect("delete moved Tag"),
        Some(root)
    );
    assert_eq!(
        commit_data(&connection, candidate).expect("cleared data"),
        None
    );
    let transaction_tags = list_tags(&connection).expect("transaction Tags");
    assert_eq!(transaction_tags.len(), 1);
    assert_eq!(transaction_tags[0].name, "ephemeral");
    drop(connection);

    let reopened = Connection::open(&path).expect("reopen recovery database");
    initialize_connection_state(&reopened).expect("reinitialize connection state");
    assert_eq!(
        commit_data(&reopened, candidate).expect("reopened Commit Data"),
        Some(r#"{"state":"staged"}"#.to_owned())
    );
    let tags = list_tags(&reopened).expect("reopened Tags");
    assert_eq!(tags.len(), 1);
    assert_eq!(tags[0].name, "staged");
    assert_eq!(tags[0].commit, candidate);
    assert!(
        integrity_check(&reopened)
            .expect("integrity after sidecar/ref rollback")
            .is_empty()
    );
    drop(reopened);
    cleanup_database(&path);
}
