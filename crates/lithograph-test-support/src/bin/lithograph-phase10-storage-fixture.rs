#![forbid(unsafe_code)]

use std::collections::BTreeMap;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::ExitCode;

use lithograph_core::cypher::Value;
use lithograph_core::query::{ExecutionOptions, QueryCursor, prepare};
use lithograph_core::storage::{
    STORAGE_FORMAT, branch_head, commit_data, create_checkpoint, create_storage_schema, create_tag,
    initialize_connection_state, initialize_root, integrity_check, resolve_version_descriptor,
    set_commit_data,
};
use lithograph_test_support::sqlite::{execute_script, extension_load_command};
use rusqlite::Connection;
use serde_json::{Value as JsonValue, json};

fn main() -> ExitCode {
    match run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            eprintln!("{error}");
            ExitCode::FAILURE
        }
    }
}

fn run() -> Result<(), Box<dyn Error>> {
    let mut args = env::args().skip(1);
    match args.next().as_deref() {
        Some("create") => {
            let path = args.next().map(PathBuf::from).ok_or(
                "usage: lithograph-phase10-storage-fixture create <database-path>",
            )?;
            create_fixture(&path)
        }
        Some("verify") => {
            let path = args.next().map(PathBuf::from).ok_or(
                "usage: lithograph-phase10-storage-fixture verify <database-path> <extension-path>",
            )?;
            let extension = args.next().map(PathBuf::from).ok_or(
                "usage: lithograph-phase10-storage-fixture verify <database-path> <extension-path>",
            )?;
            verify_fixture(&path, &extension)
        }
        _ => Err(
            "usage: lithograph-phase10-storage-fixture <create|verify> <database-path> [extension-path]"
                .into(),
        ),
    }
}

fn create_fixture(path: &Path) -> Result<(), Box<dyn Error>> {
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)?;
    }
    let connection = Connection::open(path)?;
    connection.execute_batch(
        "CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1),\
             magic TEXT NOT NULL,\
             database_id TEXT NOT NULL,\
             storage_format INTEGER NOT NULL,\
             \"semantic.embedding_cache.enabled\" INTEGER NULL \
                 CHECK(\"semantic.embedding_cache.enabled\" IN (0, 1)),\
             \"semantic.embedding_cache.max_bytes\" INTEGER NULL \
                 CHECK(\"semantic.embedding_cache.max_bytes\" > 0)\
         );",
    )?;
    connection.execute(
        "INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format) \
         VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000001010', ?1)",
        [STORAGE_FORMAT],
    )?;
    create_storage_schema(&connection)?;
    initialize_root(&connection)?;
    initialize_connection_state(&connection)?;
    execute_core(
        &connection,
        "CREATE (a:Interop {name:'alpha', value:1}), (b:Interop {name:'beta', value:2}) CREATE (a)-[:LINK {kind:'portable'}]->(b) FINISH",
    )?;
    let head = branch_head(&connection, "main")?;
    create_tag(&connection, "phase10-interop", head)?;
    set_commit_data(
        &connection,
        head,
        r#"{"fixture":"phase10-cross-platform","version":1}"#,
    )?;
    create_checkpoint(&connection, head)?;
    let issues = integrity_check(&connection)?;
    if !issues.is_empty() {
        return Err(format!("created interoperability fixture is corrupt: {issues:?}").into());
    }
    println!(
        "{}",
        json!({
            "fixture": path.display().to_string(),
            "commit": format!("commit/{}", head.to_hex()),
            "tag": "phase10-interop"
        })
    );
    Ok(())
}

fn verify_fixture(path: &Path, extension: &Path) -> Result<(), Box<dyn Error>> {
    let extension = fs::canonicalize(extension)?;
    let load = extension_load_command(&extension)?;
    let output = execute_script(
        path,
        &format!(
            "{load}\nSELECT lithograph_init();\nSELECT lithograph('MATCH (n:Interop) RETURN n.name, n.value ORDER BY n.value');\nSELECT lithograph('MATCH (:Interop)-[r:LINK]->(:Interop) RETURN r.kind');\nSELECT lithograph('CALL lithograph.tag.list() YIELD name RETURN name ORDER BY name');"
        ),
    )?;
    verify_extension_output(&output)?;
    verify_fixture_sidecars(path)?;
    println!(
        "{}",
        json!({
            "fixture": path.display().to_string(),
            "extension": extension.display().to_string(),
            "status": "ok"
        })
    );
    Ok(())
}

fn verify_extension_output(output: &str) -> Result<(), Box<dyn Error>> {
    let lines = output.lines().collect::<Vec<_>>();
    if lines.len() != 4 {
        return Err(format!("interoperability fixture returned {} lines", lines.len()).into());
    }
    let init: JsonValue = serde_json::from_str(lines[0])?;
    if init["storageFormat"] != STORAGE_FORMAT {
        return Err(
            format!("interoperability fixture storage format is not {STORAGE_FORMAT}").into(),
        );
    }
    let nodes: JsonValue = serde_json::from_str(lines[1])?;
    if nodes["rows"] != json!([["alpha", 1], ["beta", 2]]) {
        return Err(format!("interoperability Node rows differ: {}", nodes["rows"]).into());
    }
    let relationship: JsonValue = serde_json::from_str(lines[2])?;
    if relationship["rows"] != json!([["portable"]]) {
        return Err("interoperability Relationship row differs".into());
    }
    let tags: JsonValue = serde_json::from_str(lines[3])?;
    if !tags["rows"]
        .as_array()
        .is_some_and(|rows| rows.iter().any(|row| row == &json!(["phase10-interop"])))
    {
        return Err("interoperability Tag is missing".into());
    }
    Ok(())
}

fn verify_fixture_sidecars(path: &Path) -> Result<(), Box<dyn Error>> {
    let connection = Connection::open(path)?;
    let commit = resolve_version_descriptor(&connection, "tag/phase10-interop")?;
    let data =
        commit_data(&connection, commit)?.ok_or("interoperability Commit Data is missing")?;
    if serde_json::from_str::<JsonValue>(&data)?
        != json!({"fixture":"phase10-cross-platform","version":1})
    {
        return Err("interoperability Commit Data differs".into());
    }
    let issues = integrity_check(&connection)?;
    if !issues.is_empty() {
        return Err(format!("interoperability fixture integrity failed: {issues:?}").into());
    }
    Ok(())
}

fn execute_core(connection: &Connection, query: &str) -> Result<(), Box<dyn Error>> {
    let prepared = prepare(
        connection,
        query,
        BTreeMap::<String, Value>::new(),
        ExecutionOptions::default(),
    )?;
    let mut cursor = QueryCursor::new(prepared);
    loop {
        let batch = cursor.next_batch(connection, 256)?;
        if batch.done {
            cursor.complete(connection)?;
            return Ok(());
        }
    }
}
