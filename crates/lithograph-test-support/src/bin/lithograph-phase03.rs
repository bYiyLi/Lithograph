#![forbid(unsafe_code)]

use lithograph_test_support::sqlite::{FileDatabaseFixture, FixtureError, extension_load_command};
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
                serde_json::to_string_pretty(&result).expect("serialize probe")
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
        .ok_or("usage: lithograph-phase03 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let fixture = FileDatabaseFixture::new(0x0301)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;

    check_validate_success(&fixture, &load)?;
    check_parse_error(&fixture, &load)?;
    check_semantic_error(&fixture, &load)?;
    check_type_error(&fixture, &load)?;
    check_execution_uses_frontend(&fixture, &load)?;

    Ok(ProbeResult {
        phase: "03-cypher-frontend-values",
        extension: path.display().to_string(),
        checks: vec![
            "validate-success-json",
            "parse-error-category",
            "semantic-scope-error",
            "type-error-category",
            "execution-uses-validated-frontend",
        ],
    })
}

fn check_validate_success(fixture: &FileDatabaseFixture, load: &str) -> Result<(), Box<dyn Error>> {
    let output = fixture.execute_script(&format!(
        "{load}\nSELECT lithograph_validate('MATCH (n) RETURN n');"
    ))?;
    let value: Value = serde_json::from_str(&output)?;
    require(
        value["valid"].as_bool() == Some(true),
        "validate.valid must be true",
    )?;
    require(
        value["cypherProfile"].as_str() == Some("CY25-2026.08"),
        "validate must expose CY25-2026.08",
    )
}

fn check_parse_error(fixture: &FileDatabaseFixture, load: &str) -> Result<(), Box<dyn Error>> {
    assert_error(
        fixture.execute_script(&format!(
            "{load}\nSELECT lithograph_validate('MATCH (n) RETURN n,');"
        )),
        "LITHOGRAPH_PARSE_ERROR",
    )
}

fn check_semantic_error(fixture: &FileDatabaseFixture, load: &str) -> Result<(), Box<dyn Error>> {
    assert_error(
        fixture.execute_script(&format!(
            "{load}\nSELECT lithograph_validate('MATCH (n) WITH n AS x RETURN n');"
        )),
        "LITHOGRAPH_SEMANTIC_ERROR",
    )
}

fn check_type_error(fixture: &FileDatabaseFixture, load: &str) -> Result<(), Box<dyn Error>> {
    assert_error(
        fixture.execute_script(&format!(
            "{load}\nSELECT lithograph_validate('RETURN ''x'' + 1');"
        )),
        "LITHOGRAPH_TYPE_ERROR",
    )
}

fn check_execution_uses_frontend(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let output = fixture.execute_script(&format!("{load}\nSELECT lithograph('RETURN 1');"))?;
    let value: Value = serde_json::from_str(&output)?;
    require(
        value["columns"][0].as_str() == Some("1") && value["rows"][0][0].as_i64() == Some(1),
        "read execution must consume the Phase 03 frontend output",
    )
}

fn assert_error(
    result: Result<String, FixtureError>,
    expected: &str,
) -> Result<(), Box<dyn Error>> {
    let error = result.expect_err("query must fail").to_string();
    require(
        error.contains(expected),
        &format!("expected {expected}, got {error}"),
    )
}

fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
