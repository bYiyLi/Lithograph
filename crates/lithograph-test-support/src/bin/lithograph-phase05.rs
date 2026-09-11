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
        .ok_or("usage: lithograph-phase05 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let mut checks = Vec::new();

    check_scalar_write_and_summary(&load)?;
    checks.push("scalar-write-summary");
    check_failed_invocation_rolls_back(&load)?;
    checks.push("failed-invocation-rollback");
    check_late_projection_failure_rolls_back(&load)?;
    checks.push("late-projection-rollback");
    check_scalar_length_failure_rolls_back(&load)?;
    checks.push("scalar-length-rollback");
    check_autocommit_invocations_are_independent(&load)?;
    checks.push("autocommit-invocation-independence");
    check_outer_transaction_composition(&load)?;
    checks.push("outer-transaction-composition");
    check_caller_savepoint_composition(&load)?;
    checks.push("caller-savepoint-composition");
    check_rows_rejects_write(&load)?;
    checks.push("rows-read-only");
    check_graph_view_violation_rolls_back(&load)?;
    checks.push("graph-view-write-rollback");
    check_reopen_history(&load)?;
    checks.push("reopen-history");

    Ok(ProbeResult {
        phase: "05-mutation-transaction-commit",
        extension: path.display().to_string(),
        checks,
    })
}

fn initialized_fixture(seed: u64, load: &str) -> Result<FileDatabaseFixture, Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(seed)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    Ok(fixture)
}

fn check_scalar_write_and_summary(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x0501, load)?;
    let result = scalar_query(
        &fixture,
        load,
        "CREATE (n:Person {name:'Alice'}) RETURN n.name",
        "{}",
    )?;
    require(
        result["summary"]["queryType"] == "write",
        "write queryType must be write",
    )?;
    require(
        result["summary"]["counters"]["nodesCreated"] == 1,
        "CREATE must report one created Node",
    )?;
    require(
        result["summary"]["counters"]["labelsAdded"] == 1,
        "CREATE must report one Label add",
    )?;
    require(
        result["summary"]["counters"]["propertiesSet"] == 1,
        "CREATE must report one property set",
    )?;
    require(
        result["rows"] == serde_json::json!([["Alice"]]),
        "CREATE RETURN mismatch",
    )?;
    let read = scalar_query(&fixture, load, "MATCH (n:Person) RETURN n.name", "{}")?;
    require(
        read["rows"] == serde_json::json!([["Alice"]]),
        "created Node must be readable",
    )?;
    Ok(())
}

fn check_failed_invocation_rolls_back(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x0502, load)?;
    let keep = scalar_query(&fixture, load, "CREATE (:Keep) FINISH", "{}")?;
    require(
        keep["summary"]["queryType"] == "write",
        "seed write must succeed",
    )?;
    let head_before = branch_head(&Connection::open(fixture.path())?, "main")?;

    let script = format!(
        "{load}\nSELECT lithograph({});",
        sql_literal("CREATE (n:RollbackMe) SET n.bad = {nested:1} FINISH")
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("TYPE_ERROR"),
            "failed write must preserve TYPE_ERROR",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("faulting write unexpectedly succeeded".into()),
    }
    let connection = Connection::open(fixture.path())?;
    require(
        branch_head(&connection, "main")? == head_before,
        "failed invocation must not move Branch head",
    )?;
    drop(connection);
    let read = scalar_query(&fixture, load, "MATCH (n:Keep) RETURN count(n)", "{}")?;
    require(
        read["rows"] == serde_json::json!([[1]]),
        "prior invocation must survive",
    )?;
    let rolled = scalar_query(&fixture, load, "MATCH (n:RollbackMe) RETURN count(n)", "{}")?;
    require(
        rolled["rows"] == serde_json::json!([[0]]),
        "failed invocation must leave no Node",
    )?;
    Ok(())
}

fn check_late_projection_failure_rolls_back(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x050a, load)?;
    let head_before = branch_head(&Connection::open(fixture.path())?, "main")?;
    let script = format!(
        "{load}\nSELECT lithograph({});",
        sql_literal("CREATE (n:LateProjectionRollback) RETURN 1 / 0")
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("TYPE_ERROR"),
            "late projection failure must preserve TYPE_ERROR",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("late projection failure unexpectedly succeeded".into()),
    }
    let connection = Connection::open(fixture.path())?;
    require(
        branch_head(&connection, "main")? == head_before,
        "late projection failure must not move Branch head",
    )?;
    drop(connection);
    let read = scalar_query(
        &fixture,
        load,
        "MATCH (n:LateProjectionRollback) RETURN count(n)",
        "{}",
    )?;
    require(
        read["rows"] == serde_json::json!([[0]]),
        "late projection failure must leave no graph data",
    )?;
    Ok(())
}

fn check_scalar_length_failure_rolls_back(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x050b, load)?;
    let seed = format!(
        "CREATE {} FINISH",
        std::iter::repeat_n("(:EnvelopeSeed)", 16)
            .collect::<Vec<_>>()
            .join(",")
    );
    scalar_query(&fixture, load, &seed, "{}")?;
    let connection = Connection::open(fixture.path())?;
    let head_before = branch_head(&connection, "main")?;
    let commits_before: i64 =
        connection.query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })?;
    drop(connection);

    let script = format!(
        "{load}\n.limit length 1000\nSELECT lithograph('MATCH (n:EnvelopeSeed) SET n:EnvelopeSeed RETURN n');"
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => {
            require(
                stderr.contains("RESOURCE_ERROR"),
                "oversized scalar result must preserve RESOURCE_ERROR",
            )?;
        }
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("oversized scalar result unexpectedly succeeded".into()),
    }
    let connection = Connection::open(fixture.path())?;
    let commits_after: i64 =
        connection.query_row("SELECT count(*) FROM main._lithograph_commits", [], |row| {
            row.get(0)
        })?;
    require(
        branch_head(&connection, "main")? == head_before && commits_after == commits_before,
        "oversized scalar result must roll back Commit, Layer, and Branch move",
    )?;
    Ok(())
}

fn check_outer_transaction_composition(load: &str) -> Result<(), Box<dyn Error>> {
    let rollback_fixture = initialized_fixture(0x0503, load)?;
    let output = rollback_fixture.execute_script(&format!(
        "{load}\nBEGIN;\nSELECT json_extract(lithograph('CREATE (:OuterA) FINISH'), '$.summary.queryType');\nSELECT json_extract(lithograph('CREATE (:OuterB) FINISH'), '$.summary.queryType');\nROLLBACK;\nSELECT json_extract(lithograph('MATCH (n) RETURN count(n)'), '$.rows[0][0]');"
    ))?;
    require(
        output.lines().last() == Some("0"),
        "outer ROLLBACK must remove all graph invocations",
    )?;

    let commit_fixture = initialized_fixture(0x0504, load)?;
    commit_fixture.execute_script(&format!(
        "{load}\nBEGIN;\nSELECT lithograph('CREATE (:OuterA) FINISH');\nSELECT lithograph('CREATE (:OuterB) FINISH');\nCOMMIT;"
    ))?;
    let reopened = scalar_query(&commit_fixture, load, "MATCH (n) RETURN count(n)", "{}")?;
    require(
        reopened["rows"] == serde_json::json!([[2]]),
        "outer COMMIT must expose the complete write chain after reopen",
    )?;
    Ok(())
}

fn check_autocommit_invocations_are_independent(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x0508, load)?;
    let script = format!(
        "{load}\nSELECT lithograph({}), lithograph({});",
        sql_literal("CREATE (:FirstInvocation) FINISH"),
        sql_literal("CREATE (n:FailedInvocation) SET n.bad = {nested:1} FINISH")
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("TYPE_ERROR"),
            "second scalar invocation must preserve TYPE_ERROR",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("second scalar invocation unexpectedly succeeded".into()),
    }
    let first = scalar_query(
        &fixture,
        load,
        "MATCH (n:FirstInvocation) RETURN count(n)",
        "{}",
    )?;
    require(
        first["rows"] == serde_json::json!([[1]]),
        "successful first autocommit invocation must remain durable",
    )?;
    let failed = scalar_query(
        &fixture,
        load,
        "MATCH (n:FailedInvocation) RETURN count(n)",
        "{}",
    )?;
    require(
        failed["rows"] == serde_json::json!([[0]]),
        "failed second autocommit invocation must roll back independently",
    )?;
    Ok(())
}

fn check_caller_savepoint_composition(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x0509, load)?;
    let rolled_back = fixture.execute_script(&format!(
        "{load}\nSAVEPOINT caller_phase05;\nSELECT lithograph('CREATE (:CallerSavepointRollback) FINISH');\nROLLBACK TO caller_phase05;\nRELEASE caller_phase05;\nSELECT json_extract(lithograph('MATCH (n:CallerSavepointRollback) RETURN count(n)'), '$.rows[0][0]');"
    ))?;
    require(
        rolled_back.lines().last() == Some("0"),
        "caller ROLLBACK TO must remove the nested graph invocation",
    )?;

    fixture.execute_script(&format!(
        "{load}\nSAVEPOINT caller_phase05;\nSELECT lithograph('CREATE (:CallerSavepointRelease) FINISH');\nRELEASE caller_phase05;"
    ))?;
    let released = scalar_query(
        &fixture,
        load,
        "MATCH (n:CallerSavepointRelease) RETURN count(n)",
        "{}",
    )?;
    require(
        released["rows"] == serde_json::json!([[1]]),
        "caller RELEASE must persist the nested graph invocation",
    )?;
    Ok(())
}

fn check_rows_rejects_write(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x0505, load)?;
    let script = format!(
        "{load}\nSELECT row FROM lithograph_rows({});",
        sql_literal("CREATE (:RowsMustNotWrite) FINISH")
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("READ_ONLY_ADAPTER"),
            "lithograph_rows mutation must return READ_ONLY_ADAPTER",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("lithograph_rows unexpectedly executed a mutation".into()),
    }
    let read = scalar_query(
        &fixture,
        load,
        "MATCH (n:RowsMustNotWrite) RETURN count(n)",
        "{}",
    )?;
    require(
        read["rows"] == serde_json::json!([[0]]),
        "rows adapter must not write",
    )?;
    Ok(())
}

fn check_graph_view_violation_rolls_back(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x0506, load)?;
    let head_before = branch_head(&Connection::open(fixture.path())?, "main")?;
    let options = r#"{"graphView":{"requireAllLabels":["Required"]}}"#;
    let script = format!(
        "{load}\nSELECT lithograph({}, '{{}}', {});",
        sql_literal("CREATE (n) SET n:Required FINISH"),
        sql_literal(options)
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("GRAPH_VIEW_VIOLATION"),
            "view violation must use GRAPH_VIEW_VIOLATION",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("Graph View violating mutation unexpectedly succeeded".into()),
    }
    let connection = Connection::open(fixture.path())?;
    require(
        branch_head(&connection, "main")? == head_before,
        "Graph View violation must not move Branch head",
    )?;
    drop(connection);

    let relationship_script = format!(
        "{load}\nSELECT lithograph({}, '{{}}', {});",
        sql_literal("CREATE (:Required)-[:LINK]->() FINISH"),
        sql_literal(options)
    );
    match fixture.execute_script(&relationship_script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("GRAPH_VIEW_VIOLATION"),
            "Relationship with an invisible endpoint must use GRAPH_VIEW_VIOLATION",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("Graph View violating Relationship unexpectedly succeeded".into()),
    }
    let connection = Connection::open(fixture.path())?;
    require(
        branch_head(&connection, "main")? == head_before,
        "Relationship Graph View violation must not move Branch head",
    )?;
    Ok(())
}

fn check_reopen_history(load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = initialized_fixture(0x0507, load)?;
    let first = scalar_query(&fixture, load, "CREATE (n:Versioned {v:1}) FINISH", "{}")?;
    let first_commit = first["summary"]["commit"]
        .as_str()
        .ok_or("first write is missing commit")?
        .to_owned();
    scalar_query(
        &fixture,
        load,
        "MATCH (n:Versioned) SET n.v = 2 FINISH",
        "{}",
    )?;
    let current = scalar_query(&fixture, load, "MATCH (n:Versioned) RETURN n.v", "{}")?;
    require(
        current["rows"] == serde_json::json!([[2]]),
        "reopened current head mismatch",
    )?;
    let historical = scalar_query(
        &fixture,
        load,
        "MATCH (n:Versioned) RETURN n.v",
        &format!(r#"{{"at":"{first_commit}"}}"#),
    )?;
    require(
        historical["rows"] == serde_json::json!([[1]]),
        "reopened historical Commit mismatch",
    )?;
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
