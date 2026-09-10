#![forbid(unsafe_code)]

use lithograph_test_support::sqlite::{FileDatabaseFixture, extension_load_command};
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::error::Error;
use std::fs;
use std::path::PathBuf;
use std::process::ExitCode;

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
        .ok_or("usage: lithograph-phase02 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let mut checks = Vec::new();

    check_init_reopen(&load)?;
    checks.push("root-main-reopen");
    check_phase01_bootstrap_migration(&load)?;
    checks.push("phase01-bootstrap-migration");
    check_partial_storage_rejected(&load)?;
    checks.push("partial-storage-rejected");
    check_integrity_corruption_surface(&load)?;
    checks.push("integrity-corruption-surface");

    Ok(ProbeResult {
        phase: "02-version-aware-storage-core",
        extension: path.display().to_string(),
        checks,
    })
}

fn check_init_reopen(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0201)?;
    let first_output = fixture.execute_script(&format!(
        "{load}\nSELECT lithograph_init(); SELECT lithograph_integrity_check();"
    ))?;
    let first_lines: Vec<_> = first_output.lines().collect();
    require(first_lines.len() == 2, "first init must return two rows")?;
    let first = parse_json(first_lines[0], "first init")?;
    let first_integrity = parse_json(first_lines[1], "first integrity")?;
    require(
        first_integrity["ok"].as_bool() == Some(true),
        "fresh storage integrity must pass",
    )?;
    let database_id = string_field(&first, "databaseId")?.to_owned();
    let root = string_field(&first, "root")?.to_owned();
    require(
        first["branch"].as_str() == Some("main"),
        "fresh branch must be main",
    )?;

    let reopened_output = fixture.execute_script(&format!(
        "{load}\nSELECT lithograph_init(); SELECT lithograph_version(); SELECT lithograph_integrity_check();"
    ))?;
    let reopened_lines: Vec<_> = reopened_output.lines().collect();
    require(reopened_lines.len() == 3, "reopen must return three rows")?;
    let reopened = parse_json(reopened_lines[0], "reopened init")?;
    let version = parse_json(reopened_lines[1], "reopened version")?;
    let integrity = parse_json(reopened_lines[2], "reopened integrity")?;
    require(
        reopened["databaseId"].as_str() == Some(database_id.as_str()),
        "databaseId must remain stable across reopen",
    )?;
    require(
        reopened["root"].as_str() == Some(root.as_str()),
        "Root Commit must remain stable across reopen",
    )?;
    require(
        version["databaseId"].as_str() == Some(database_id.as_str()),
        "version must preserve databaseId across reopen",
    )?;
    require(
        integrity["ok"].as_bool() == Some(true),
        "reopened integrity must pass",
    )?;
    Ok(())
}

fn check_phase01_bootstrap_migration(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0202)?;
    let database_id = "11111111-1111-4111-8111-111111111111";
    fixture.execute_script(&format!(
        "CREATE TABLE main._lithograph_meta(\
            id INTEGER PRIMARY KEY CHECK(id = 1),\
            magic TEXT NOT NULL,\
            database_id TEXT NOT NULL,\
            storage_format INTEGER NOT NULL\
        );\
        INSERT INTO main._lithograph_meta VALUES(1, 'lithograph-format-v1', '{database_id}', 1);"
    ))?;
    let output = fixture.execute_script(&format!(
        "{load}\nSELECT lithograph_init(); SELECT lithograph_integrity_check();"
    ))?;
    let lines: Vec<_> = output.lines().collect();
    require(lines.len() == 2, "migration must return two rows")?;
    let init = parse_json(lines[0], "bootstrap migration init")?;
    let integrity = parse_json(lines[1], "bootstrap migration integrity")?;
    require(
        init["databaseId"].as_str() == Some(database_id),
        "Phase 01 bootstrap migration must preserve databaseId",
    )?;
    require(
        init["branch"].as_str() == Some("main"),
        "migration must create main",
    )?;
    require(
        string_field(&init, "root")?.len() == 64,
        "migration must create Root",
    )?;
    require(
        integrity["ok"].as_bool() == Some(true),
        "migrated integrity must pass",
    )?;
    Ok(())
}

fn check_partial_storage_rejected(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0203)?;
    fixture.execute_script(
        "CREATE TABLE main._lithograph_meta(\
            id INTEGER PRIMARY KEY CHECK(id = 1),\
            magic TEXT NOT NULL, database_id TEXT NOT NULL, storage_format INTEGER NOT NULL\
        );\
        INSERT INTO main._lithograph_meta VALUES(1, 'lithograph-format-v1', '22222222-2222-4222-8222-222222222222', 1);\
        CREATE TABLE main._lithograph_sequences(kind INTEGER PRIMARY KEY, next_id INTEGER NOT NULL);",
    )?;
    let error = fixture
        .execute_script(&format!("{load}\nSELECT lithograph_init();"))
        .expect_err("partial storage must be rejected")
        .to_string();
    require(
        error.contains("LITHOGRAPH_STORAGE_ERROR"),
        "partial format-1 storage must fail as STORAGE_ERROR",
    )?;
    Ok(())
}

fn check_integrity_corruption_surface(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0204)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    fixture.execute_script(
        "UPDATE main._lithograph_branches SET commit_id = randomblob(32) WHERE name = 'main';",
    )?;
    let output =
        fixture.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    let integrity = parse_json(&output, "corrupt branch integrity")?;
    require(
        integrity["ok"].as_bool() == Some(false),
        "dangling branch must fail integrity",
    )?;
    let serialized = integrity.to_string();
    require(
        serialized.contains("refs.dangling_branch"),
        "integrity result must expose dangling-branch finding",
    )?;
    Ok(())
}

fn parse_json(input: &str, context: &str) -> Result<Value, Box<dyn Error>> {
    serde_json::from_str(input).map_err(|error| format!("{context}: invalid JSON: {error}").into())
}

fn string_field<'a>(value: &'a Value, name: &str) -> Result<&'a str, Box<dyn Error>> {
    value[name]
        .as_str()
        .ok_or_else(|| format!("{name} must be a string").into())
}

fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
