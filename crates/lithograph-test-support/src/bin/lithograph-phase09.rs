#![forbid(unsafe_code)]

use lithograph_core::storage::{
    CommitMetadata, HashId, LayerBuilder, allocate_node_id, branch_head, commit_layer,
    create_storage_schema, initialize_root, load_snapshot_state,
};
use lithograph_test_support::sqlite::{FileDatabaseFixture, FixtureError, extension_load_command};
use rusqlite::{Connection, params};
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

const FORMAT1_ROOT: &str = "23e60794878d0ce1fa5bb1d102507a6589ea7a8cd84d530a8d77302d771b119a";

#[derive(Serialize)]
struct ProbeResult {
    phase: &'static str,
    extension: String,
    checks: Vec<&'static str>,
}

fn main() -> ExitCode {
    match run() {
        Ok(result) => {
            println!(
                "{}",
                serde_json::to_string_pretty(&result).expect("probe result must serialize")
            );
            ExitCode::SUCCESS
        }
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<ProbeResult, Box<dyn Error>> {
    let path = env::args()
        .nth(1)
        .map(PathBuf::from)
        .ok_or("usage: lithograph-phase09 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let mut checks = Vec::new();

    check_format1_migration(&load)?;
    checks.push("format1-to-format2-migration");
    check_failed_migration_rolls_back(&load)?;
    checks.push("migration-failure-rollback");
    check_metadata_update_fault_rolls_back(&load)?;
    checks.push("migration-finalize-fault-rollback");
    check_corrupt_history_refuses_migration(&load)?;
    checks.push("migration-corrupt-history-refusal");
    check_too_new_format_is_rejected(&load)?;
    checks.push("too-new-format-rejection");
    check_session_restart_and_adapter_boundaries(&load)?;
    checks.push("merge-session-restart-and-adapters");

    Ok(ProbeResult {
        phase: "09-version-control-operations",
        extension: path.display().to_string(),
        checks,
    })
}

fn check_format1_migration(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0901)?;
    seed_format1_database(fixture.path(), false)?;
    let before = Connection::open(fixture.path())?;
    let old_root = HashId::from_hex(FORMAT1_ROOT)?;
    require(
        branch_head(&before, "main")? == old_root,
        "format-1 fixture must point main at the frozen format-1 Root Commit",
    )?;
    let root_snapshot = load_snapshot_state(&before, old_root)?;
    let node = allocate_node_id(&before)?;
    let mut layer = LayerBuilder::default();
    layer.add_node(node)?;
    let old_head = commit_layer(
        &before,
        "main",
        old_root,
        None,
        &layer,
        &CommitMetadata {
            author: Some("phase10-migration".to_owned()),
            message: Some("preserve format-1 history".to_owned()),
            committed_at: 1,
        },
    )?;
    let head_snapshot = load_snapshot_state(&before, old_head)?;
    drop(before);

    let output = fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    let init: Value = serde_json::from_str(&output)?;
    require(
        init["storageFormat"] == 2,
        "migration must advance metadata to storage format 2",
    )?;
    require(
        init["root"] == FORMAT1_ROOT,
        "migration must preserve the existing Root Commit id",
    )?;
    verify_format2_state(
        fixture.path(),
        old_root,
        old_head,
        &root_snapshot,
        &head_snapshot,
    )
}

fn verify_format2_state(
    path: &std::path::Path,
    old_root: HashId,
    old_head: HashId,
    root_snapshot: &lithograph_core::storage::SnapshotState,
    head_snapshot: &lithograph_core::storage::SnapshotState,
) -> Result<(), Box<dyn Error>> {
    let after = Connection::open(path)?;
    require(
        branch_head(&after, "main")? == old_head,
        "migration must preserve the main Branch head",
    )?;
    require(
        load_snapshot_state(&after, old_root)? == *root_snapshot,
        "migration must preserve the existing Root Snapshot",
    )?;
    require(
        load_snapshot_state(&after, old_head)? == *head_snapshot,
        "migration must preserve the existing descendant Snapshot",
    )?;
    let parent: Vec<u8> = after.query_row(
        "SELECT parent1 FROM main._lithograph_commits WHERE id=?1",
        [old_head.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    require(
        parent == old_root.as_bytes().as_slice(),
        "migration must preserve the existing Commit DAG edge",
    )?;
    let format: i64 = after.query_row(
        "SELECT storage_format FROM main._lithograph_meta WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    require(format == 2, "migration must persist storage format 2")?;
    for table in [
        "_lithograph_commit_data",
        "_lithograph_tags",
        "_lithograph_merge_sessions",
        "_lithograph_merge_resolutions",
    ] {
        let count: i64 =
            after.query_row(&format!("SELECT count(*) FROM main.{table}"), [], |row| {
                row.get(0)
            })?;
        require(
            count == 0,
            "new format-2 sidecar/session tables must start empty",
        )?;
    }
    Ok(())
}

fn check_failed_migration_rolls_back(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0902)?;
    seed_format1_database(fixture.path(), true)?;
    let script = format!("{load}\nSELECT lithograph_init();");
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { .. }) => {}
        Err(error) => return Err(error.into()),
        Ok(_) => {
            return Err(
                "format-1 migration unexpectedly succeeded with a conflicting Tag table".into(),
            );
        }
    }
    let connection = Connection::open(fixture.path())?;
    let format: i64 = connection.query_row(
        "SELECT storage_format FROM main._lithograph_meta WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    require(
        format == 1,
        "failed migration must leave metadata at format 1",
    )?;
    let commit_data_exists: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name='_lithograph_commit_data')",
        [],
        |row| row.get(0),
    )?;
    require(
        !commit_data_exists,
        "failed migration must rollback sidecar tables created before the failure",
    )
}

fn check_metadata_update_fault_rolls_back(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0904)?;
    seed_format1_database(fixture.path(), false)?;
    let old_root = HashId::from_hex(FORMAT1_ROOT)?;
    let root_snapshot = install_migration_metadata_fault(fixture.path(), old_root)?;
    let script = format!("{load}\nSELECT lithograph_init();");
    expect_migration_failure(&fixture, &script)?;
    verify_metadata_fault_rollback(fixture.path(), old_root, &root_snapshot)?;

    let output = fixture.execute_script(&script)?;
    let init: Value = serde_json::from_str(&output)?;
    require(
        init["storageFormat"] == 2,
        "migration must succeed when retried after metadata fault removal",
    )?;
    let after = Connection::open(fixture.path())?;
    require(
        branch_head(&after, "main")? == old_root,
        "retried migration must preserve the format-1 Branch head",
    )?;
    require(
        load_snapshot_state(&after, old_root)? == root_snapshot,
        "retried migration must preserve the format-1 Root Snapshot",
    )
}

fn install_migration_metadata_fault(
    path: &std::path::Path,
    old_root: HashId,
) -> Result<lithograph_core::storage::SnapshotState, Box<dyn Error>> {
    let connection = Connection::open(path)?;
    let root_snapshot = load_snapshot_state(&connection, old_root)?;
    connection.execute_batch(
        "CREATE TRIGGER phase10_fail_format_update \
         BEFORE UPDATE OF storage_format ON main._lithograph_meta \
         WHEN NEW.storage_format = 2 \
         BEGIN \
           SELECT RAISE(ABORT, 'phase10 migration metadata fault'); \
         END;",
    )?;
    Ok(root_snapshot)
}

fn expect_migration_failure(
    fixture: &FileDatabaseFixture,
    script: &str,
) -> Result<(), Box<dyn Error>> {
    match fixture.execute_script(script) {
        Err(FixtureError::Sqlite { .. }) => {}
        Err(error) => return Err(error.into()),
        Ok(_) => {
            return Err("format-1 migration unexpectedly survived metadata fault".into());
        }
    }
    Ok(())
}

fn verify_metadata_fault_rollback(
    path: &std::path::Path,
    old_root: HashId,
    root_snapshot: &lithograph_core::storage::SnapshotState,
) -> Result<(), Box<dyn Error>> {
    let recovery = Connection::open(path)?;
    let format: i64 = recovery.query_row(
        "SELECT storage_format FROM main._lithograph_meta WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    require(
        format == 1,
        "metadata-update fault must leave storage format at 1",
    )?;
    require(
        branch_head(&recovery, "main")? == old_root,
        "metadata-update fault must preserve the format-1 Branch head",
    )?;
    require(
        load_snapshot_state(&recovery, old_root)? == *root_snapshot,
        "metadata-update fault must preserve the format-1 Root Snapshot",
    )?;
    for table in [
        "_lithograph_commit_data",
        "_lithograph_tags",
        "_lithograph_merge_sessions",
        "_lithograph_merge_resolutions",
    ] {
        let exists: bool = recovery.query_row(
            "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema WHERE type='table' AND name=?1)",
            [table],
            |row| row.get(0),
        )?;
        require(
            !exists,
            "metadata-update fault must rollback every format-2 sidecar table",
        )?;
    }
    recovery.execute_batch("DROP TRIGGER main.phase10_fail_format_update")?;
    Ok(())
}

fn check_corrupt_history_refuses_migration(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0905)?;
    seed_format1_database(fixture.path(), false)?;
    let root = HashId::from_hex(FORMAT1_ROOT)?;
    let connection = Connection::open(fixture.path())?;
    let (layer_id, original_hash): (i64, Vec<u8>) = connection.query_row(
        "SELECT layer_id, hash FROM main._lithograph_commits c \
         JOIN main._lithograph_layers l ON l.id = c.layer_id WHERE c.id=?1",
        [root.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    connection.execute(
        "UPDATE main._lithograph_layers SET hash=zeroblob(32) WHERE id=?1",
        [layer_id],
    )?;
    drop(connection);

    let script = format!("{load}\nSELECT lithograph_init();");
    require_corrupt_migration_refusal(&fixture, &script)?;
    repair_corrupt_history_fixture(fixture.path(), layer_id, original_hash)?;

    let output = fixture.execute_script(&script)?;
    let init: Value = serde_json::from_str(&output)?;
    require(
        init["storageFormat"] == 2 && init["root"] == FORMAT1_ROOT,
        "repaired format-1 history must migrate without changing Root Commit identity",
    )
}

fn require_corrupt_migration_refusal(
    fixture: &FileDatabaseFixture,
    script: &str,
) -> Result<(), Box<dyn Error>> {
    match fixture.execute_script(script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("LITHOGRAPH_STORAGE_ERROR")
                && stderr.contains("history.commit_hash_mismatch"),
            &format!(
                "corrupt history migration must report an immutable history hash mismatch: {stderr}"
            ),
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("corrupt format-1 history unexpectedly migrated".into()),
    }
    Ok(())
}

fn repair_corrupt_history_fixture(
    path: &std::path::Path,
    layer_id: i64,
    original_hash: Vec<u8>,
) -> Result<(), Box<dyn Error>> {
    let recovery = Connection::open(path)?;
    let format: i64 = recovery.query_row(
        "SELECT storage_format FROM main._lithograph_meta WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    require(
        format == 1,
        "corrupt-history refusal must rollback the storage-format metadata update",
    )?;
    let sidecar_exists: bool = recovery.query_row(
        "SELECT EXISTS(SELECT 1 FROM main.sqlite_schema \
         WHERE type='table' AND name='_lithograph_commit_data')",
        [],
        |row| row.get(0),
    )?;
    require(
        !sidecar_exists,
        "corrupt-history refusal must rollback format-2 sidecar creation",
    )?;
    recovery.execute(
        "UPDATE main._lithograph_layers SET hash=?1 WHERE id=?2",
        params![original_hash, layer_id],
    )?;
    Ok(())
}

fn check_too_new_format_is_rejected(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0906)?;
    seed_format1_database(fixture.path(), false)?;
    let connection = Connection::open(fixture.path())?;
    connection.execute(
        "UPDATE main._lithograph_meta SET storage_format=3 WHERE id=1",
        [],
    )?;
    drop(connection);

    let script = format!("{load}\nSELECT lithograph_init();");
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("FORMAT_TOO_NEW"),
            "too-new format must fail with the stable FORMAT_TOO_NEW category",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("too-new storage format unexpectedly initialized".into()),
    }
    let connection = Connection::open(fixture.path())?;
    let format: i64 = connection.query_row(
        "SELECT storage_format FROM main._lithograph_meta WHERE id=1",
        [],
        |row| row.get(0),
    )?;
    require(
        format == 3,
        "too-new refusal must not mutate database metadata",
    )
}

fn check_session_restart_and_adapter_boundaries(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0903)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    scalar_query(&fixture, load, "CREATE (:Item {name:'base'}) FINISH", "{}")?;
    scalar_query(
        &fixture,
        load,
        "CALL lithograph.branch.create('feature') YIELD name RETURN name",
        "{}",
    )?;
    scalar_query(
        &fixture,
        load,
        "MATCH (n:Item) SET n.name='ours' FINISH",
        "{}",
    )?;
    scalar_query(
        &fixture,
        load,
        "MATCH (n:Item) SET n.name='theirs' FINISH",
        r#"{"branch":"feature"}"#,
    )?;
    let started = scalar_query(
        &fixture,
        load,
        "CALL lithograph.merge.start('branch/feature') YIELD session, revision, status RETURN session, revision, status",
        "{}",
    )?;
    require(
        started["rows"][0][2] == "conflicted",
        "restart fixture must start with one merge conflict",
    )?;
    let session = started["rows"][0][0]
        .as_str()
        .ok_or("merge.start must return session id")?;
    let conflicts = scalar_query(
        &fixture,
        load,
        &format!(
            "CALL lithograph.merge.conflicts('{session}', 10) YIELD conflictId RETURN conflictId"
        ),
        "{}",
    )?;
    let conflict_id = conflicts["rows"][0][0]
        .as_str()
        .ok_or("merge.conflicts must return conflict id")?;
    scalar_query(
        &fixture,
        load,
        &format!(
            "CALL lithograph.merge.resolve('{session}', 1, [{{conflictId:'{conflict_id}', choice:'ours'}}]) YIELD revision RETURN revision"
        ),
        "{}",
    )?;

    // Each scalar_query invocation uses a new sqlite3 process/connection, so
    // this proves persisted Session + resolution state survives restart.
    let candidate_options = format!(r#"{{"mergeSession":{{"id":"{session}","revision":2}}}}"#);
    let candidate = scalar_query(
        &fixture,
        load,
        "MATCH (n:Item) RETURN n.name",
        &candidate_options,
    )?;
    require(
        candidate["rows"] == serde_json::json!([["ours"]]),
        "candidate read after connection restart returned unexpected state",
    )?;
    require(
        candidate["summary"]["commit"].is_null(),
        "Merge Session candidate summary must not expose a Commit identity",
    )?;
    require(
        candidate["summary"]["mergeSession"]["id"] == session,
        "candidate summary must identify the pinned Merge Session",
    )?;

    check_adapter_boundaries(&fixture, load)?;
    abort_session_and_check_restart(&fixture, load, session)
}

fn check_adapter_boundaries(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let rows_script = format!(
        "{load}\nSELECT row FROM lithograph_rows({});",
        sql_literal("CALL lithograph.branch.checkout('feature') YIELD name RETURN name")
    );
    match fixture.execute_script(&rows_script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("READ_ONLY_ADAPTER"),
            "lithograph_rows must reject checkout/version connection-state mutation",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("lithograph_rows unexpectedly executed Branch checkout".into()),
    }

    let graph_view_script = format!(
        "{load}\nSELECT lithograph({}, '{{}}', {});",
        sql_literal("CALL lithograph.branch.list() YIELD name RETURN name"),
        sql_literal(r#"{"graphView":{"requireAllLabels":["Item"]}}"#)
    );
    match fixture.execute_script(&graph_view_script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("INVALID_ARGUMENT"),
            "Version Procedure with graphView must return INVALID_ARGUMENT",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("Version Procedure unexpectedly accepted graphView".into()),
    }
    Ok(())
}

fn abort_session_and_check_restart(
    fixture: &FileDatabaseFixture,
    load: &str,
    session: &str,
) -> Result<(), Box<dyn Error>> {
    scalar_query(
        fixture,
        load,
        &format!("CALL lithograph.merge.abort('{session}', 2) YIELD session RETURN session"),
        "{}",
    )?;
    let get_script = format!(
        "{load}\nSELECT lithograph({}, '{{}}', '{{}}');",
        sql_literal(&format!(
            "CALL lithograph.merge.get('{session}') YIELD session RETURN session"
        ))
    );
    match fixture.execute_script(&get_script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("MERGE_SESSION_NOT_FOUND"),
            "aborted Session must not survive restart",
        ),
        Err(error) => Err(error.into()),
        Ok(_) => Err("aborted Merge Session unexpectedly remained readable".into()),
    }
}

fn seed_format1_database(
    path: &std::path::Path,
    conflicting_tag_table: bool,
) -> Result<(), Box<dyn Error>> {
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1),\
             magic TEXT NOT NULL,\
             database_id TEXT NOT NULL,\
             storage_format INTEGER NOT NULL\
         );\
         INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format)\
         VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000901', 1);",
    )?;
    create_storage_schema(&connection)?;
    let current_root = initialize_root(&connection)?.root;
    let (layer_id, schema_hash): (i64, Vec<u8>) = connection.query_row(
        "SELECT layer_id, schema_hash FROM main._lithograph_commits WHERE id=?1",
        [current_root.as_bytes().as_slice()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    connection.execute("DELETE FROM main._lithograph_branches", [])?;
    connection.execute("DELETE FROM main._lithograph_commits", [])?;
    let old_root = HashId::from_hex(FORMAT1_ROOT)?;
    connection.execute(
        "INSERT INTO main._lithograph_commits(\
             id, format_version, parent1, parent2, layer_id, schema_hash, author, message, committed_at\
         ) VALUES(?1, 1, NULL, NULL, ?2, ?3, NULL, NULL, 0)",
        params![old_root.as_bytes().as_slice(), layer_id, schema_hash],
    )?;
    connection.execute(
        "INSERT INTO main._lithograph_branches(name, commit_id) VALUES('main', ?1)",
        [old_root.as_bytes().as_slice()],
    )?;
    for table in [
        "_lithograph_merge_resolutions",
        "_lithograph_merge_sessions",
        "_lithograph_tags",
        "_lithograph_commit_data",
    ] {
        connection.execute_batch(&format!("DROP TABLE main.{table}"))?;
    }
    if conflicting_tag_table {
        connection.execute_batch("CREATE TABLE main._lithograph_tags(name TEXT PRIMARY KEY)")?;
    }
    Ok(())
}

fn scalar_query(
    fixture: &FileDatabaseFixture,
    load: &str,
    query: &str,
    options: &str,
) -> Result<Value, Box<dyn Error>> {
    let text = fixture.execute_script(&format!(
        "{load}\nSELECT lithograph({}, '{{}}', {});",
        sql_literal(query),
        sql_literal(options)
    ))?;
    serde_json::from_str(&text).map_err(Into::into)
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
