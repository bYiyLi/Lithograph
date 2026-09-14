use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, allocate_node_id, branch_head, capture_allocation_state,
    clear_commit_data, commit_data, commit_exists, commit_layer, create_checkpoint,
    create_storage_schema, create_tag, delete_tag, initialize_connection_state, initialize_root,
    integrity_check, list_tags, move_tag, set_commit_data,
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
