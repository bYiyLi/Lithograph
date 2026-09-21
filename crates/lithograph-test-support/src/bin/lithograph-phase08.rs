#![forbid(unsafe_code)]

use lithograph_test_support::sqlite::{FileDatabaseFixture, FixtureError, extension_load_command};
use serde::Serialize;
use serde_json::Value;
use std::env;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
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
        .ok_or("usage: lithograph-phase08 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let fixture = FileDatabaseFixture::new(0x0801)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    let mut checks = Vec::new();

    check_search_and_fulltext(&fixture, &load)?;
    checks.push("search-fulltext");
    check_sql_bridge_transaction_execution(&fixture, &load)?;
    checks.push("sql-bridge-transaction-execution");
    check_rows_transaction_and_external_io(&fixture, &load)?;
    checks.push("rows-transaction-external-io");
    check_scalar_load_csv(&fixture, &load)?;
    checks.push("scalar-load-csv");

    Ok(ProbeResult {
        phase: "08-search-ingestion",
        extension: path.display().to_string(),
        checks,
    })
}

fn check_search_and_fulltext(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    scalar_query(
        fixture,
        load,
        "CREATE (:Doc {name:'A', text:'graph database', embedding:[1.0,0.0]}), (:Doc {name:'B', text:'other topic', embedding:[0.0,1.0]}) FINISH",
    )?;
    scalar_query(
        fixture,
        load,
        "CREATE FULLTEXT INDEX doc_text FOR (n:Doc) ON EACH [n.text]",
    )?;
    scalar_query(
        fixture,
        load,
        "CREATE VECTOR INDEX doc_embedding FOR (n:Doc) ON (n.embedding)",
    )?;
    let fulltext = scalar_query(
        fixture,
        load,
        "CALL db.index.fulltext.queryNodes('doc_text', 'graph') YIELD node RETURN node.name",
    )?;
    require(
        fulltext["rows"] == serde_json::json!([["A"]]),
        "full-text query through SQL Bridge returned unexpected rows",
    )?;
    let vector = scalar_query(
        fixture,
        load,
        "MATCH (n:Doc) SEARCH n IN (VECTOR INDEX doc_embedding FOR [1.0,0.0] LIMIT 1) RETURN n.name",
    )?;
    require(
        vector["rows"] == serde_json::json!([["A"]]),
        "VECTOR SEARCH through SQL Bridge returned unexpected rows",
    )
}

fn check_sql_bridge_transaction_execution(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let query = "UNWIND [1,2] AS value CALL (value) { CREATE (:SqlBridgeBatch {value:value}) } IN TRANSACTIONS OF 1 ROWS FINISH";
    let result = scalar_query(fixture, load, query)?;
    require(
        result["summary"]["queryType"] == "write",
        "SQL Bridge transaction-owning query must complete as a write",
    )?;
    let count = scalar_query(fixture, load, "MATCH (n:SqlBridgeBatch) RETURN count(n)")?;
    require(
        count["rows"] == serde_json::json!([[2]]),
        "SQL Bridge transaction batches did not persist committed rows",
    )
}

fn check_rows_transaction_and_external_io(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let transaction = "UNWIND [1,2] AS value CALL (value) { RETURN value AS innerValue } IN TRANSACTIONS OF 1 ROWS RETURN innerValue ORDER BY innerValue";
    let events = fixture.execute_script(&format!(
        "{load}\nSELECT group_concat(event, ',') FROM lithograph_rows({});",
        sql_literal(transaction)
    ))?;
    require(
        events.trim() == "columns,row,row,summary",
        "lithograph_rows transaction-owning query returned an unexpected event lifecycle",
    )?;

    let csv = fixture.directory().join("phase08-rows.csv");
    fs::write(&csv, "name\nRowsAlice\nRowsBob\n")?;
    let uri = local_file_uri(&csv);
    let load_csv =
        format!("LOAD CSV WITH HEADERS FROM '{uri}' AS row RETURN row.name ORDER BY row.name");
    let events = fixture.execute_script(&format!(
        "{load}\nSELECT group_concat(event, ',') FROM lithograph_rows({});",
        sql_literal(&load_csv)
    ))?;
    require(
        events.trim() == "columns,row,row,summary",
        "lithograph_rows external-I/O query returned an unexpected event lifecycle",
    )?;
    Ok(())
}

fn check_scalar_load_csv(fixture: &FileDatabaseFixture, load: &str) -> Result<(), Box<dyn Error>> {
    let csv = fixture.directory().join("phase08.csv");
    fs::write(&csv, "name\nAlice\nBob\n")?;
    let uri = local_file_uri(&csv);
    let query =
        format!("LOAD CSV WITH HEADERS FROM '{uri}' AS row RETURN row.name ORDER BY row.name");
    let result = scalar_query(fixture, load, &query)?;
    require(
        result["rows"] == serde_json::json!([["Alice"], ["Bob"]]),
        "ordinary SQL Bridge LOAD CSV returned unexpected rows",
    )?;

    let missing =
        "LOAD CSV FROM 'file:///definitely/not/a/lithograph/phase08/missing.csv' AS row RETURN row";
    let script = format!(
        "{load}\nSELECT lithograph({}, '{{}}', '{{}}');",
        sql_literal(missing)
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("IO_ERROR"),
            "SQL Bridge LOAD CSV I/O failures must use IO_ERROR",
        ),
        Err(error) => Err(error.into()),
        Ok(_) => Err("SQL Bridge unexpectedly accepted a missing LOAD CSV source".into()),
    }
}

fn scalar_query(
    fixture: &FileDatabaseFixture,
    load: &str,
    query: &str,
) -> Result<Value, Box<dyn Error>> {
    let text = fixture.execute_script(&format!(
        "{load}\nSELECT lithograph({}, '{{}}', '{{}}');",
        sql_literal(query)
    ))?;
    serde_json::from_str(&text).map_err(Into::into)
}

fn sql_literal(value: &str) -> String {
    format!("'{}'", value.replace('\'', "''"))
}

fn local_file_uri(path: &Path) -> String {
    let normalized = path.to_string_lossy().replace('\\', "/");
    if cfg!(windows) {
        format!("file:///{}", normalized.trim_start_matches('/'))
    } else {
        format!("file://{normalized}")
    }
}

fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
