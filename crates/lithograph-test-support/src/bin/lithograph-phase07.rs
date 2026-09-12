#![forbid(unsafe_code)]

use lithograph_core::storage::branch_head;
use lithograph_test_support::sqlite::{FileDatabaseFixture, FixtureError, extension_load_command};
use rusqlite::Connection;
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
        .ok_or("usage: lithograph-phase07 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let fixture = FileDatabaseFixture::new(0x0701)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    let mut checks = Vec::new();

    check_schema_commit_and_show(&fixture, &load)?;
    checks.push("schema-commit-show");
    check_constraint_rollback(&fixture, &load)?;
    checks.push("constraint-rollback");
    check_standard_indexes_and_graph_view(&fixture, &load)?;
    checks.push("standard-index-graph-view");
    check_graph_view_rejects_schema_ddl(&fixture, &load)?;
    checks.push("schema-ddl-graph-view-rollback");

    Ok(ProbeResult {
        phase: "07-schema-constraint-standard-index",
        extension: path.display().to_string(),
        checks,
    })
}

fn check_schema_commit_and_show(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let result = scalar_query(
        fixture,
        load,
        "ALTER CURRENT GRAPH TYPE SET { (:Person => :Resident {name :: STRING NOT NULL}) }",
        "{}",
    )?;
    require(
        result["summary"]["queryType"] == "schema"
            && result["summary"]["commit"].as_str().is_some(),
        "Graph Type DDL must return a schema summary with a Commit",
    )?;
    let shown = scalar_query(
        fixture,
        load,
        "SHOW NODE EXISTENCE CONSTRAINT YIELD type RETURN type ORDER BY type",
        "{}",
    )?;
    require(
        shown["rows"] == serde_json::json!([["NODE_LABEL_EXISTENCE"], ["NODE_PROPERTY_EXISTENCE"]]),
        "SHOW CONSTRAINTS filter must expose Graph Type dependent constraints",
    )
}

fn check_constraint_rollback(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    scalar_query(
        fixture,
        load,
        "CREATE CONSTRAINT person_name_unique FOR (n:Person) REQUIRE n.name IS UNIQUE",
        "{}",
    )?;
    scalar_query(
        fixture,
        load,
        "CREATE (:Person:Resident {name:'Alice'}) FINISH",
        "{}",
    )?;
    let before = branch_head(&Connection::open(fixture.path())?, "main")?;
    let script = format!(
        "{load}\nSELECT lithograph({});",
        sql_literal("CREATE (:Person:Resident {name:'Alice'}) FINISH")
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("CONSTRAINT_ERROR"),
            "duplicate constrained write must preserve CONSTRAINT_ERROR",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("constraint-violating write unexpectedly succeeded".into()),
    }
    require(
        branch_head(&Connection::open(fixture.path())?, "main")? == before,
        "constraint failure must not move the Branch head",
    )
}

fn check_standard_indexes_and_graph_view(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    scalar_query(
        fixture,
        load,
        "CREATE (:Person:Resident:Visible {name:'Bob', age:2}), (:Person:Resident:Hidden {name:'Carol', age:3}) FINISH",
        "{}",
    )?;
    scalar_query(
        fixture,
        load,
        "CREATE RANGE INDEX person_age FOR (n:Person) ON (n.age)",
        "{}",
    )?;
    let explain = scalar_query(
        fixture,
        load,
        "EXPLAIN MATCH (n:Person) WHERE n.age >= 2 RETURN n.name",
        "{}",
    )?;
    require(
        explain["rows"][0][0]
            .as_str()
            .is_some_and(|plan| plan.contains("IndexSeek")),
        "range predicate must plan an IndexSeek through the extension",
    )?;
    let visible = scalar_query(
        fixture,
        load,
        "MATCH (n:Person) WHERE n.age >= 2 RETURN n.name ORDER BY n.name",
        r#"{"graphView":{"requireAllLabels":["Visible"]}}"#,
    )?;
    require(
        visible["rows"] == serde_json::json!([["Bob"]]),
        "Graph View must hide index candidates outside the visible subgraph",
    )
}

fn check_graph_view_rejects_schema_ddl(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let before = branch_head(&Connection::open(fixture.path())?, "main")?;
    let options = r#"{"graphView":{"excludeAnyLabels":["Hidden"]}}"#;
    let script = format!(
        "{load}\nSELECT lithograph({}, '{{}}', {});",
        sql_literal("CREATE TEXT INDEX person_name_text FOR (n:Person) ON (n.name)"),
        sql_literal(options)
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("INVALID_ARGUMENT"),
            "Schema DDL with graphView must preserve INVALID_ARGUMENT",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("Schema DDL with graphView unexpectedly succeeded".into()),
    }
    require(
        branch_head(&Connection::open(fixture.path())?, "main")? == before,
        "rejected Schema DDL must not move the Branch head",
    )
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
