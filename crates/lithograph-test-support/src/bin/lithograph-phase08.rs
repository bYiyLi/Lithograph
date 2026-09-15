#![forbid(unsafe_code)]

use lithograph_core::storage::branch_head;
use lithograph_test_support::sqlite::{FileDatabaseFixture, FixtureError, extension_load_command};
use rusqlite::Connection;
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
    check_sql_bridge_transaction_boundary(&fixture, &load)?;
    checks.push("sql-bridge-transaction-boundary");
    check_rows_rejects_transaction_and_external_io(&fixture, &load)?;
    checks.push("rows-no-transaction-or-external-io");
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

fn check_sql_bridge_transaction_boundary(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let before = branch_head(&Connection::open(fixture.path())?, "main")?;
    let query = "UNWIND [1] AS value CALL (value) { CREATE (:SqlBridgeMustNotBatch) } IN TRANSACTIONS FINISH";
    let script = format!("{load}\nSELECT lithograph({});", sql_literal(query));
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("TRANSACTION_BOUNDARY_REQUIRED"),
            "SQL Bridge transaction-owning query must return TRANSACTION_BOUNDARY_REQUIRED",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("SQL Bridge unexpectedly executed transaction-owning Cypher".into()),
    }
    require(
        branch_head(&Connection::open(fixture.path())?, "main")? == before,
        "rejected SQL Bridge transaction query must not move Branch head",
    )
}

fn check_rows_rejects_transaction_and_external_io(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    for query in [
        "UNWIND [1] AS value CALL (value) { RETURN value AS innerValue } IN TRANSACTIONS RETURN innerValue",
        "LOAD CSV FROM 'file:///definitely/not/a/lithograph/phase08/file.csv' AS row RETURN row",
    ] {
        let script = format!(
            "{load}\nSELECT row FROM lithograph_rows({});",
            sql_literal(query)
        );
        match fixture.execute_script(&script) {
            Err(FixtureError::Sqlite { stderr, .. }) => require(
                stderr.contains("READ_ONLY_ADAPTER"),
                "lithograph_rows must reject transaction-owning and external-I/O Cypher before side effects",
            )?,
            Err(error) => return Err(error.into()),
            Ok(_) => return Err("lithograph_rows unexpectedly executed a forbidden query".into()),
        }
    }
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
