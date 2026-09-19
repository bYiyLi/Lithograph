use lithograph_test_support::sqlite::FileDatabaseFixture;
use serde_json::Value as JsonValue;
use std::env;
use std::error::Error;
use std::io::Write;
use std::path::Path;
use std::process::{Command, Stdio};

pub(super) fn semantic_node_create(
    name: &str,
    label: &str,
    property: &str,
    provider: &str,
    provider_config_json: &str,
    dimensions: usize,
    similarity: &str,
) -> String {
    format!(
        "CALL db.index.semantic.createNodeIndex('{name}',['{label}'],'{property}',{{provider:'{provider}',providerConfig:{},dimensions:{dimensions},similarity:'{similarity}'}})",
        json_as_cypher(provider_config_json)
    )
}

pub(super) fn semantic_relationship_create(
    name: &str,
    relationship_type: &str,
    property: &str,
    provider: &str,
    provider_config_json: &str,
    dimensions: usize,
    similarity: &str,
) -> String {
    format!(
        "CALL db.index.semantic.createRelationshipIndex('{name}',['{relationship_type}'],'{property}',{{provider:'{provider}',providerConfig:{},dimensions:{dimensions},similarity:'{similarity}'}})",
        json_as_cypher(provider_config_json)
    )
}

pub(super) fn json_as_cypher(value: &str) -> String {
    value
        .replace("\"base_url\"", "base_url")
        .replace("\"api_key_env\"", "api_key_env")
        .replace("\"api_key\"", "api_key")
        .replace("\"model\"", "model")
        .replace("\"send_dimensions\"", "send_dimensions")
        .replace("\"encoding_format\"", "encoding_format")
        .replace("\"user\"", "user")
        .replace("\"organization\"", "organization")
        .replace("\"project\"", "project")
        .replace("\"headers\"", "headers")
        .replace("\"timeout_ms\"", "timeout_ms")
        .replace("\"max_retries\"", "max_retries")
        .replace("\"batch_size\"", "batch_size")
        .replace("\"semantic_identity\"", "semantic_identity")
        .replace("\"variant\"", "variant")
        .replace("\"validate\"", "validate")
        .replace("\"invalid\"", "invalid")
        .replace("\"fail\"", "fail")
}

pub(super) fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

pub(super) fn branch_head_text(
    fixture: &FileDatabaseFixture,
    lithograph: &str,
) -> Result<String, Box<dyn Error>> {
    let output = fixture.execute_script(&format!(
        "{lithograph}\nSELECT 'head=' || json_extract(lithograph('CALL lithograph.commit.get(''branch/main'') YIELD commit RETURN commit'), '$.rows[0][0]');"
    ))?;
    Ok(prefixed_value(&output, "head=")?.to_owned())
}

pub(super) fn require_persistent_cache_empty(
    fixture: &FileDatabaseFixture,
) -> Result<(), Box<dyn Error>> {
    let output = fixture
        .execute_script("SELECT 'cache=' || count(*) FROM main._lithograph_embedding_cache;")?;
    require_line(&output, "cache=0")
}

pub(super) fn execute_script_allowing_failure(
    database: &Path,
    script: &str,
) -> Result<(bool, String, String), Box<dyn Error>> {
    execute_target_allowing_failure(&database.to_string_lossy(), script)
}

pub(super) fn execute_target_allowing_failure(
    database: &str,
    script: &str,
) -> Result<(bool, String, String), Box<dyn Error>> {
    let sqlite = env::var_os("LITHOGRAPH_SQLITE3").unwrap_or_else(|| "sqlite3".into());
    let mut child = Command::new(sqlite)
        .arg("-batch")
        .arg("-noheader")
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .ok_or("sqlite3 stdin unavailable")?
        .write_all(script.as_bytes())?;
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

pub(super) fn execute_readonly_script_allowing_failure(
    database: &Path,
    script: &str,
) -> Result<(bool, String, String), Box<dyn Error>> {
    let sqlite = env::var_os("LITHOGRAPH_SQLITE3").unwrap_or_else(|| "sqlite3".into());
    let mut child = Command::new(sqlite)
        .arg("-batch")
        .arg("-noheader")
        .arg("-readonly")
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .ok_or("sqlite3 stdin unavailable")?
        .write_all(script.as_bytes())?;
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    Ok((
        output.status.success(),
        String::from_utf8_lossy(&output.stdout).trim().to_owned(),
        String::from_utf8_lossy(&output.stderr).trim().to_owned(),
    ))
}

pub(super) fn prefixed_json(value: &str, prefix: &str) -> Result<JsonValue, Box<dyn Error>> {
    Ok(serde_json::from_str(prefixed_value(value, prefix)?)?)
}

pub(super) fn prefixed_value<'a>(value: &'a str, prefix: &str) -> Result<&'a str, Box<dyn Error>> {
    value
        .lines()
        .find_map(|line| line.trim().strip_prefix(prefix))
        .ok_or_else(|| format!("missing {prefix:?} in {value:?}").into())
}

pub(super) fn require_line(value: &str, expected: &str) -> Result<(), Box<dyn Error>> {
    require(
        value.lines().any(|line| line.trim() == expected),
        &format!("expected output line {expected:?}, got {value:?}"),
    )
}

pub(super) fn require_positive_pair(value: &str, prefix: &str) -> Result<(), Box<dyn Error>> {
    let payload = prefixed_value(value, prefix)?;
    let values = payload
        .split(':')
        .map(str::parse::<i64>)
        .collect::<Result<Vec<_>, _>>()?;
    require(
        values.len() == 2 && values[0] > 0 && values[1] > 0,
        &format!("expected positive counter pair for {prefix}, got {payload}"),
    )
}

pub(super) fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
