#![forbid(unsafe_code)]

use lithograph_test_support::sqlite::{
    FileDatabaseFixture, FixtureError, InMemoryDatabaseFixture, extension_load_command,
};
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
        .ok_or("usage: lithograph-phase01 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let mut checks = Vec::new();

    check_load_is_registration_only(&load)?;
    checks.push("load-registration-only");

    check_uninitialized_contract(&load)?;
    checks.push("not-initialized-contract");

    check_init_and_metadata(&load)?;
    checks.push("init-metadata-idempotence");

    check_outer_transaction_rollback(&load)?;
    checks.push("outer-transaction-rollback");

    check_reserved_object_collision(&load)?;
    checks.push("reserved-object-collision");

    check_reserved_namespace_integrity(&load)?;
    checks.push("reserved-namespace-integrity");

    check_temp_schema_shadowing(&load)?;
    checks.push("main-schema-isolation");

    check_format_boundaries(&load)?;
    checks.push("format-boundaries");

    check_metadata_integrity_contract(&load)?;
    checks.push("metadata-integrity-detection");

    check_safety_flags(&load)?;
    checks.push("direct-only-and-innocuous");

    check_execution_boundary(&load)?;
    checks.push("execution-adapters-share-current-engine");

    check_stable_sql_errors(&load)?;
    checks.push("stable-sql-error-contract");

    Ok(ProbeResult {
        phase: "01-sqlite-extension-boundary",
        extension: path.display().to_string(),
        checks,
    })
}

fn check_load_is_registration_only(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0101)?;
    fixture.execute_script("CREATE TABLE user_table(v INTEGER); PRAGMA user_version = 77;")?;
    let before = fixture.execute_script(
        "SELECT type || ':' || name FROM sqlite_schema ORDER BY type, name; PRAGMA user_version;",
    )?;
    let after = fixture.execute_script(&format!(
        "{load}\nSELECT type || ':' || name FROM sqlite_schema ORDER BY type, name; PRAGMA user_version;"
    ))?;
    require_equal(&before, &after, ".load must not change persistent schema")?;
    Ok(())
}

fn check_uninitialized_contract(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = InMemoryDatabaseFixture::new(0x0102);
    let version = fixture.execute_script(&format!("{load}\nSELECT lithograph_version();"))?;
    let version = parse_json(&version, "lithograph_version before init")?;
    require(
        version["databaseId"].is_null(),
        "databaseId must be null before init",
    )?;
    require(
        version["storageFormat"]["current"].is_null(),
        "current storage format must be null before init",
    )?;

    assert_sqlite_error(
        fixture.execute_script(&format!("{load}\nSELECT lithograph('RETURN 1');")),
        "LITHOGRAPH_NOT_INITIALIZED",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!("{load}\nSELECT lithograph_validate('RETURN 1');")),
        "LITHOGRAPH_NOT_INITIALIZED",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!(
            "{load}\nSELECT * FROM lithograph_rows('RETURN 1');"
        )),
        "LITHOGRAPH_NOT_INITIALIZED",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();")),
        "LITHOGRAPH_NOT_INITIALIZED",
    )?;
    Ok(())
}

fn check_init_and_metadata(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0103)?;
    fixture.execute_script("CREATE TABLE user_table(v INTEGER); PRAGMA user_version = 77;")?;
    let output = fixture.execute_script(&format!(
        "{load}\n\
         SELECT lithograph_init();\n\
         SELECT lithograph_init();\n\
         SELECT lithograph_version();\n\
         SELECT lithograph_integrity_check();\n\
         PRAGMA user_version;"
    ))?;
    let lines: Vec<_> = output.lines().collect();
    require_equal(&lines.len(), &5, "init probe must return five lines")?;
    let first = parse_json(lines[0], "first init")?;
    let second = parse_json(lines[1], "second init")?;
    let version = parse_json(lines[2], "version after init")?;
    let integrity = parse_json(lines[3], "integrity after init")?;
    check_initialized_json(&first, &second, &version, &integrity, lines[4])?;
    let required_storage = fixture.execute_script(
        "SELECT count(*) FROM sqlite_schema WHERE name IN ('_lithograph_meta', '_lithograph_commits', '_lithograph_branches', '_lithograph_layers');",
    )?;
    require_equal(
        &required_storage,
        &"4".to_string(),
        "Phase 02 init must expose the required canonical storage tables",
    )?;
    Ok(())
}

fn check_initialized_json(
    first: &Value,
    second: &Value,
    version: &Value,
    integrity: &Value,
    user_version: &str,
) -> Result<(), Box<dyn Error>> {
    let database_id = first["databaseId"]
        .as_str()
        .ok_or("first init must return databaseId")?;
    require_equal(
        &second["databaseId"].as_str(),
        &Some(database_id),
        "idempotent init must preserve databaseId",
    )?;
    require_equal(
        &version["databaseId"].as_str(),
        &Some(database_id),
        "version must report initialized databaseId",
    )?;
    require_equal(
        &version["storageFormat"]["current"].as_i64(),
        &Some(1),
        "version must report storage format 1",
    )?;
    let root = first["root"]
        .as_str()
        .ok_or("init must return the Root Commit after Phase 02")?;
    require(
        is_lower_hex_commit(root),
        "Root Commit must be 64 lowercase hex characters",
    )?;
    require_equal(
        &second["root"].as_str(),
        &Some(root),
        "idempotent init must preserve the Root Commit",
    )?;
    require_equal(
        &first["branch"].as_str(),
        &Some("main"),
        "init must return the main Branch after Phase 02",
    )?;
    require_equal(
        &integrity["ok"].as_bool(),
        &Some(true),
        "metadata integrity must pass",
    )?;
    require_equal(
        &user_version,
        &"77",
        "init must not consume PRAGMA user_version",
    )?;
    Ok(())
}

fn is_lower_hex_commit(value: &str) -> bool {
    if value.len() != 64 || !value.is_ascii() || value != value.to_ascii_lowercase() {
        return false;
    }
    let (high, low) = value.split_at(32);
    u128::from_str_radix(high, 16).is_ok() && u128::from_str_radix(low, 16).is_ok()
}

fn check_outer_transaction_rollback(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0104)?;
    let output = fixture.execute_script(&format!(
        "{load}\nBEGIN; SELECT lithograph_init(); ROLLBACK;\n\
         SELECT count(*) FROM sqlite_schema WHERE name = '_lithograph_meta';"
    ))?;
    let lines: Vec<_> = output.lines().collect();
    require_equal(&lines.len(), &2, "rollback probe must return two lines")?;
    parse_json(lines[0], "init inside outer transaction")?;
    require_equal(&lines[1], &"0", "outer transaction rollback must undo init")?;
    Ok(())
}

fn check_reserved_object_collision(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0105)?;
    fixture.execute_script("CREATE TABLE _lithograph_user_object(v INTEGER);")?;
    assert_sqlite_error(
        fixture.execute_script(&format!("{load}\nSELECT lithograph_init();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    let state = fixture.execute_script(
        "SELECT name FROM sqlite_schema WHERE name LIKE '_lithograph_%' ORDER BY name;",
    )?;
    require_equal(
        &state,
        &"_lithograph_user_object".to_string(),
        "collision handling must not overwrite or add internal objects",
    )?;

    let lookalike = FileDatabaseFixture::new(0x0109)?;
    lookalike.execute_script("CREATE TABLE xlithograph_user_object(v INTEGER);")?;
    let init = lookalike.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    parse_json(&init, "init with non-reserved lookalike object")?;
    let state = lookalike.execute_script(
        "SELECT name FROM sqlite_schema WHERE name IN ('xlithograph_user_object', '_lithograph_meta') ORDER BY name;",
    )?;
    require_equal(
        &state,
        &"_lithograph_meta\nxlithograph_user_object".to_string(),
        "only the exact _lithograph_ prefix must be reserved",
    )?;

    let case_variant = FileDatabaseFixture::new(0x0110)?;
    case_variant.execute_script("CREATE TABLE _LITHOGRAPH_future(v INTEGER);")?;
    assert_sqlite_error(
        case_variant.execute_script(&format!("{load}\nSELECT lithograph_init();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    assert_sqlite_error(
        case_variant.execute_script(&format!("{load}\nSELECT lithograph_version();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    let integrity =
        case_variant.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    assert_integrity_failure(&integrity, "case-variant reserved collision")?;
    Ok(())
}

fn check_reserved_namespace_integrity(load: &str) -> Result<(), Box<dyn Error>> {
    check_renamed_metadata_is_corrupt(load)?;
    check_unexpected_reserved_object_is_corrupt(load)?;
    check_unexpected_internal_child_objects_are_corrupt(load)?;
    check_temp_internal_triggers_are_rejected(load)?;
    Ok(())
}

fn check_renamed_metadata_is_corrupt(load: &str) -> Result<(), Box<dyn Error>> {
    let renamed = FileDatabaseFixture::new(0x0111)?;
    renamed.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    renamed
        .execute_script("ALTER TABLE main._lithograph_meta RENAME TO _lithograph_meta_broken;")?;
    assert_sqlite_error(
        renamed.execute_script(&format!("{load}\nSELECT lithograph_version();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    let integrity =
        renamed.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    assert_integrity_failure(&integrity, "renamed metadata table")?;
    assert_sqlite_error(
        renamed.execute_script(&format!("{load}\nSELECT lithograph_init();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    Ok(())
}

fn check_unexpected_reserved_object_is_corrupt(load: &str) -> Result<(), Box<dyn Error>> {
    let extra_object = FileDatabaseFixture::new(0x0112)?;
    extra_object.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    extra_object.execute_script("CREATE TABLE main._lithograph_unexpected(v INTEGER);")?;
    let integrity =
        extra_object.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    assert_integrity_failure(&integrity, "unexpected reserved object")?;
    assert_sqlite_error(
        extra_object.execute_script(&format!("{load}\nSELECT lithograph_version();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    assert_sqlite_error(
        extra_object.execute_script(&format!("{load}\nSELECT lithograph_init();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    Ok(())
}

fn check_unexpected_internal_child_objects_are_corrupt(load: &str) -> Result<(), Box<dyn Error>> {
    let extra_trigger = FileDatabaseFixture::new(0x0113)?;
    extra_trigger.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    extra_trigger.execute_script(
        "CREATE TRIGGER main.user_probe AFTER UPDATE ON _lithograph_meta BEGIN SELECT 1; END;",
    )?;
    let integrity =
        extra_trigger.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    assert_integrity_failure(&integrity, "unexpected internal-table trigger")?;
    assert_sqlite_error(
        extra_trigger.execute_script(&format!("{load}\nSELECT lithograph_version();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;

    let extra_index = FileDatabaseFixture::new(0x0116)?;
    extra_index.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    extra_index
        .execute_script("CREATE INDEX main.user_meta_index ON _lithograph_meta(database_id);")?;
    let integrity =
        extra_index.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    assert_integrity_failure(&integrity, "unexpected internal-table index")?;
    assert_sqlite_error(
        extra_index.execute_script(&format!("{load}\nSELECT lithograph_version();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    Ok(())
}

fn check_temp_internal_triggers_are_rejected(load: &str) -> Result<(), Box<dyn Error>> {
    let temp_trigger = InMemoryDatabaseFixture::new(0x0117);
    let integrity = temp_trigger.execute_script(&format!(
        "{load}\n\
         SELECT lithograph_init();\n\
         CREATE TEMP TRIGGER temp_meta_probe AFTER UPDATE ON main._lithograph_meta BEGIN SELECT 1; END;\n\
         SELECT lithograph_integrity_check();"
    ))?;
    let integrity = integrity
        .lines()
        .last()
        .ok_or("TEMP trigger integrity probe returned no rows")?;
    assert_integrity_failure(integrity, "TEMP trigger on internal table")?;

    let temp_trigger_version = InMemoryDatabaseFixture::new(0x0118);
    assert_sqlite_error(
        temp_trigger_version.execute_script(&format!(
            "{load}\n\
             SELECT lithograph_init();\n\
             CREATE TEMP TRIGGER temp_meta_probe AFTER UPDATE ON main._lithograph_meta BEGIN SELECT 1; END;\n\
             SELECT lithograph_version();"
        )),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;

    let temp_trigger_init = InMemoryDatabaseFixture::new(0x0119);
    assert_sqlite_error(
        temp_trigger_init.execute_script(&format!(
            "{load}\n\
             SELECT lithograph_init();\n\
             CREATE TEMP TRIGGER temp_meta_probe AFTER UPDATE ON main._lithograph_meta BEGIN SELECT 1; END;\n\
             SELECT lithograph_init();"
        )),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;

    let preinit_temp_trigger = InMemoryDatabaseFixture::new(0x011a);
    assert_sqlite_error(
        preinit_temp_trigger.execute_script(&format!(
            "{load}\n\
             CREATE TEMP TABLE _lithograph_meta(v INTEGER);\n\
             CREATE TEMP TRIGGER temp_meta_probe AFTER UPDATE ON temp._lithograph_meta BEGIN SELECT 1; END;\n\
             SELECT lithograph_init();"
        )),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    Ok(())
}

fn check_temp_schema_shadowing(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x010a)?;
    let output = fixture.execute_script(&format!(
        "{load}\n\
         CREATE TEMP TABLE _lithograph_meta(id INTEGER, magic TEXT, database_id TEXT, storage_format INTEGER);\n\
         SELECT lithograph_init();\n\
         SELECT count(*) FROM main._lithograph_meta;\n\
         SELECT count(*) FROM temp._lithograph_meta;"
    ))?;
    let lines: Vec<_> = output.lines().collect();
    require_equal(
        &lines.len(),
        &3,
        "TEMP shadow probe must return three lines",
    )?;
    let init = parse_json(lines[0], "init with TEMP shadow")?;
    let database_id = init["databaseId"]
        .as_str()
        .ok_or("TEMP shadow init must return databaseId")?;
    require_equal(
        &lines[1],
        &"1",
        "init must persist the marker in main._lithograph_meta",
    )?;
    require_equal(&lines[2], &"0", "init must not write the TEMP shadow table")?;

    let reopened = fixture.execute_script(&format!(
        "{load}\nSELECT lithograph_version(); SELECT lithograph_integrity_check();"
    ))?;
    let lines: Vec<_> = reopened.lines().collect();
    require_equal(&lines.len(), &2, "reopen probe must return two lines")?;
    let version = parse_json(lines[0], "version after TEMP shadow disappears")?;
    let integrity = parse_json(lines[1], "integrity after TEMP shadow disappears")?;
    require_equal(
        &version["databaseId"].as_str(),
        &Some(database_id),
        "databaseId must persist in main after the TEMP connection closes",
    )?;
    require_equal(
        &integrity["ok"].as_bool(),
        &Some(true),
        "TEMP shadow must not leave a half-initialized main database",
    )?;

    let attached = InMemoryDatabaseFixture::new(0x0115);
    let output = attached.execute_script(&format!(
        "{load}\n\
         ATTACH ':memory:' AS aux;\n\
         CREATE TABLE aux._lithograph_meta(id INTEGER, magic TEXT, database_id TEXT, storage_format INTEGER);\n\
         SELECT lithograph_init();\n\
         SELECT count(*) FROM main._lithograph_meta;\n\
         SELECT count(*) FROM aux._lithograph_meta;"
    ))?;
    let lines: Vec<_> = output.lines().collect();
    require_equal(
        &lines.len(),
        &3,
        "attached-schema shadow probe must return three lines",
    )?;
    parse_json(lines[0], "init with attached-schema shadow")?;
    require_equal(&lines[1], &"1", "init must persist metadata only in main")?;
    require_equal(
        &lines[2],
        &"0",
        "init must not write attached-schema metadata",
    )?;
    Ok(())
}

fn check_format_boundaries(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0106)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    fixture.execute_script(
        "UPDATE _lithograph_meta SET storage_format = 2 WHERE id = 1;\
         CREATE TABLE main._lithograph_future(v INTEGER);",
    )?;

    let version = fixture.execute_script(&format!("{load}\nSELECT lithograph_version();"))?;
    let version = parse_json(&version, "version on newer format")?;
    require_equal(
        &version["storageFormat"]["current"].as_i64(),
        &Some(2),
        "version must remain readable on a newer format with unknown future schema objects",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!("{load}\nSELECT lithograph_init();")),
        "LITHOGRAPH_FORMAT_TOO_NEW",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();")),
        "LITHOGRAPH_FORMAT_TOO_NEW",
    )?;

    let lower = FileDatabaseFixture::new(0x0114)?;
    lower.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    lower.execute_script("UPDATE _lithograph_meta SET storage_format = 0 WHERE id = 1;")?;
    assert_sqlite_error(
        lower.execute_script(&format!("{load}\nSELECT lithograph_version();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    let integrity =
        lower.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    assert_integrity_failure(&integrity, "below-minimum storage format")?;
    assert_sqlite_error(
        lower.execute_script(&format!("{load}\nSELECT lithograph_init();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    Ok(())
}

fn check_metadata_integrity_contract(load: &str) -> Result<(), Box<dyn Error>> {
    let marker_fixture = FileDatabaseFixture::new(0x010b)?;
    marker_fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    marker_fixture
        .execute_script("UPDATE main._lithograph_meta SET magic = 'corrupt' WHERE id = 1;")?;
    let integrity =
        marker_fixture.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    assert_integrity_failure(&integrity, "corrupt metadata marker")?;
    assert_sqlite_error(
        marker_fixture.execute_script(&format!("{load}\nSELECT lithograph_version();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;

    let schema_fixture = FileDatabaseFixture::new(0x010c)?;
    schema_fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    schema_fixture.execute_script(
        "ALTER TABLE main._lithograph_meta RENAME TO _lithograph_meta_backup;\
         CREATE TABLE main._lithograph_meta(id INTEGER, magic TEXT, database_id TEXT, storage_format INTEGER);\
         INSERT INTO main._lithograph_meta SELECT * FROM main._lithograph_meta_backup;\
         INSERT INTO main._lithograph_meta VALUES(2, 'junk', 'junk', 999);\
         DROP TABLE main._lithograph_meta_backup;",
    )?;
    let integrity =
        schema_fixture.execute_script(&format!("{load}\nSELECT lithograph_integrity_check();"))?;
    assert_integrity_failure(&integrity, "corrupt metadata schema")?;
    assert_sqlite_error(
        schema_fixture.execute_script(&format!("{load}\nSELECT lithograph_version();")),
        "LITHOGRAPH_STORAGE_ERROR",
    )?;
    Ok(())
}

fn check_safety_flags(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = InMemoryDatabaseFixture::new(0x0107);
    assert_sqlite_error(
        fixture.execute_script(&format!(
            "{load}\nCREATE VIEW init_view AS SELECT lithograph_init() AS value; SELECT value FROM init_view;"
        )),
        "unsafe use of lithograph_init()",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!(
            "{load}\nCREATE VIEW rows_view AS SELECT * FROM lithograph_rows('RETURN 1'); SELECT * FROM rows_view;"
        )),
        "unsafe use of virtual table \"lithograph_rows\"",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!(
            "{load}\nCREATE VIRTUAL TABLE persisted_rows USING lithograph_rows;"
        )),
        "no such module: lithograph_rows",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!(
            "{load}\n\
             SELECT lithograph_init();\n\
             CREATE TABLE trigger_target(v INTEGER);\n\
             CREATE TRIGGER execute_trigger AFTER INSERT ON trigger_target BEGIN SELECT lithograph('RETURN 1'); END;\n\
             INSERT INTO trigger_target VALUES(1);"
        )),
        "unsafe use of lithograph()",
    )?;

    let version_from_view = fixture.execute_script(&format!(
        "{load}\nCREATE VIEW version_view AS SELECT lithograph_version() AS value; SELECT json_valid(value) FROM version_view;"
    ))?;
    require_equal(
        &version_from_view,
        &"1".to_string(),
        "innocuous version function must be usable from a view",
    )?;
    let integrity_from_view = fixture.execute_script(&format!(
        "{load}\n\
         SELECT lithograph_init();\n\
         CREATE VIEW integrity_view AS SELECT lithograph_integrity_check() AS value;\n\
         SELECT json_extract(value, '$.ok') FROM integrity_view;"
    ))?;
    let lines: Vec<_> = integrity_from_view.lines().collect();
    require_equal(
        &lines.last(),
        &Some(&"1"),
        "read-only integrity function must be usable from a view",
    )?;
    Ok(())
}

fn check_execution_boundary(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0108)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;

    let scalar = fixture.execute_script(&format!("{load}\nSELECT lithograph('RETURN 1');"))?;
    let scalar = parse_json(&scalar, "scalar read execution")?;
    require_equal(
        &scalar["columns"][0].as_str(),
        &Some("1"),
        "scalar execution must expose the RETURN column",
    )?;
    require_equal(
        &scalar["rows"][0][0].as_i64(),
        &Some(1),
        "scalar execution must return the projected value",
    )?;
    let validation =
        fixture.execute_script(&format!("{load}\nSELECT lithograph_validate('RETURN 1');"))?;
    let validation = parse_json(&validation, "frontend validation")?;
    require_equal(
        &validation["valid"].as_bool(),
        &Some(true),
        "frontend validation must succeed without executing the query",
    )?;
    require_equal(
        &validation["cypherProfile"].as_str(),
        &Some("CY25-2026.08"),
        "frontend validation must expose the frozen Cypher profile",
    )?;
    let rows = fixture.execute_script(&format!(
        "{load}\nSELECT ordinal, columns, row FROM lithograph_rows('RETURN 1');"
    ))?;
    require(
        rows.contains("0|[\"1\"]|[1]"),
        "rows execution must stream the same column and value contract",
    )?;
    assert_sqlite_error(
        fixture.execute_script(&format!(
            "{load}\nSELECT lithograph('RETURN 1', '[]', '{{}}');"
        )),
        "LITHOGRAPH_INVALID_ARGUMENT",
    )?;
    Ok(())
}

fn check_stable_sql_errors(load: &str) -> Result<(), Box<dyn Error>> {
    let uninitialized = InMemoryDatabaseFixture::new(0x010d);
    assert_sqlite_error(
        uninitialized.execute_script(&format!("{load}\nSELECT lithograph('');")),
        "LITHOGRAPH_INVALID_ARGUMENT",
    )?;

    let fixture = FileDatabaseFixture::new(0x010e)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    for script in [
        "SELECT lithograph_validate(1);",
        "SELECT lithograph_validate(NULL);",
        "SELECT * FROM lithograph_rows(1);",
        "SELECT * FROM lithograph_rows('RETURN 1', 1, '{}');",
        "SELECT * FROM lithograph_rows('RETURN 1', '{}', 1);",
        "SELECT * FROM lithograph_rows;",
    ] {
        assert_sqlite_error(
            fixture.execute_script(&format!("{load}\n{script}")),
            "LITHOGRAPH_INVALID_ARGUMENT",
        )?;
    }
    Ok(())
}

fn assert_integrity_failure(input: &str, label: &str) -> Result<(), Box<dyn Error>> {
    let value = parse_json(input, label)?;
    require_equal(
        &value["ok"].as_bool(),
        &Some(false),
        "corrupt readable metadata must return ok=false",
    )?;
    let errors = value["errors"]
        .as_array()
        .ok_or("integrity errors must be an array")?;
    require(!errors.is_empty(), "integrity errors must not be empty")?;
    require(
        errors.iter().all(|error| {
            error["category"].as_str().is_some()
                && error["sqliteCode"].as_i64().is_some()
                && error.get("line").is_some()
                && error.get("column").is_some()
        }),
        "integrity errors must use the structured error JSON shape",
    )?;
    Ok(())
}

fn parse_json(input: &str, label: &str) -> Result<Value, Box<dyn Error>> {
    serde_json::from_str(input)
        .map_err(|error| format!("{label} returned invalid JSON: {error}").into())
}

fn assert_sqlite_error(
    result: Result<String, FixtureError>,
    needle: &str,
) -> Result<(), Box<dyn Error>> {
    match result {
        Err(FixtureError::Sqlite { stderr, .. }) if stderr.contains(needle) => Ok(()),
        Err(error) => {
            Err(format!("expected SQLite error containing {needle:?}, got {error}").into())
        }
        Ok(output) => Err(format!(
            "expected SQLite error containing {needle:?}, command succeeded with {output:?}"
        )
        .into()),
    }
}

fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}

fn require_equal<T>(actual: &T, expected: &T, message: &str) -> Result<(), Box<dyn Error>>
where
    T: PartialEq + std::fmt::Debug,
{
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{message}: expected {expected:?}, got {actual:?}").into())
    }
}
