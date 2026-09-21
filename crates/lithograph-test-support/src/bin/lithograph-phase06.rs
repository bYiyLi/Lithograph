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
        .ok_or("usage: lithograph-phase06 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let fixture = FileDatabaseFixture::new(0x0601)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    let mut checks = Vec::new();

    check_composition_and_aggregation(&fixture, &load)?;
    checks.push("composition-aggregation");
    check_rows_program_adapter(&fixture, &load)?;
    checks.push("rows-program-adapter");
    check_versioned_program_write_and_rollback(&fixture, &load)?;
    checks.push("program-write-rollback");
    check_advanced_path_graph_view(&fixture, &load)?;
    checks.push("advanced-path-graph-view");
    check_registry_and_typed_values(&fixture, &load)?;
    checks.push("registry-temporal-vector-unicode");

    Ok(ProbeResult {
        phase: "06-cypher25-query-completeness",
        extension: path.display().to_string(),
        checks,
    })
}

fn check_composition_and_aggregation(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let result = scalar_query(
        fixture,
        load,
        "UNWIND [1, 2, 3] AS x WITH x WHERE x > 1 CALL (x) { RETURN x * 2 AS y } RETURN collect(y) AS values, sum(y) AS total",
        "{}",
    )?;
    require(
        result["rows"] == serde_json::json!([[[4, 6], 10]]),
        "composition/aggregation result mismatch",
    )
}

fn check_rows_program_adapter(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let query = "FOR x IN [3, 1, 2] RETURN x ORDER BY x";
    let output = fixture.execute_script(&format!(
        "{load}\nSELECT json_extract(data, '$[0]') FROM lithograph_rows({}) WHERE event='row' ORDER BY ordinal;",
        sql_literal(query)
    ))?;
    require(
        output.lines().collect::<Vec<_>>() == ["1", "2", "3"],
        "lithograph_rows must execute and stream the Phase 06 program result",
    )
}

fn check_versioned_program_write_and_rollback(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let result = scalar_query(
        fixture,
        load,
        "UNWIND [1, 2] AS value CREATE (node:Visible {value:value}) FOREACH (extra IN [value + 10] | SET node.extra = extra) WITH node RETURN node.value, node.extra ORDER BY node.value",
        "{}",
    )?;
    require(
        result["rows"] == serde_json::json!([[1, 11], [2, 12]]),
        "composed write must return staged values",
    )?;
    require(
        result["summary"]["counters"]["nodesCreated"] == 2
            && result["summary"]["counters"]["propertiesSet"] == 4,
        "composed write counters mismatch",
    )?;

    let script = format!(
        "{load}\nSELECT lithograph({});",
        sql_literal("CREATE (node:RolledBack06) SET node[42] = 1 FINISH")
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("TYPE_ERROR") && stderr.contains("dynamic property key"),
            "dynamic property failure must preserve the public type error",
        )?,
        Err(error) => return Err(error.into()),
        Ok(_) => return Err("invalid dynamic property write unexpectedly succeeded".into()),
    }
    let rolled_back = scalar_query(
        fixture,
        load,
        "MATCH (node:RolledBack06) RETURN count(node)",
        "{}",
    )?;
    require(
        rolled_back["rows"] == serde_json::json!([[0]]),
        "failed Phase 06 write must roll back",
    )
}

fn check_advanced_path_graph_view(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    scalar_query(
        fixture,
        load,
        "MATCH (a:Visible {value:1}), (b:Visible {value:2}) CREATE (a)-[:R]->(b), (b)-[:R]->(:Hidden {value:3}) FINISH",
        "{}",
    )?;
    let query = "MATCH (start:Visible {value:1}) RETURN COUNT { MATCH (start)-[:R]->+(target) RETURN target } AS reachable";
    let full = scalar_query(fixture, load, query, "{}")?;
    require(
        full["rows"] == serde_json::json!([[2]]),
        "full graph quantified path count mismatch",
    )?;
    let view = r#"{"graphView":{"requireAllLabels":["Visible"]}}"#;
    let filtered = scalar_query(fixture, load, query, view)?;
    require(
        filtered["rows"] == serde_json::json!([[1]]),
        "Graph View must remain active inside quantified expression subqueries",
    )
}

fn check_registry_and_typed_values(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let function = scalar_query(
        fixture,
        load,
        "SHOW FUNCTIONS YIELD name WHERE name = 'vector.similarity.euclidean' RETURN name",
        "{}",
    )?;
    require(
        function["rows"] == serde_json::json!([["vector.similarity.euclidean"]]),
        "SHOW FUNCTIONS must use the runtime registry",
    )?;
    let labels = scalar_query(
        fixture,
        load,
        "CALL db.labels() YIELD label WHERE label = 'Visible' RETURN count(label)",
        "{}",
    )?;
    require(
        labels["rows"] == serde_json::json!([[1]]),
        "current-graph procedure result mismatch",
    )?;
    let typed = scalar_query(
        fixture,
        load,
        r#"RETURN format(datetime('2024-07-01T12:00:00+02:00[Europe/Paris]')) AS zoned, s"值={1 + 2}😀" AS rendered, vector_distance(vector([0.0, 0.0], 2, FLOAT64), vector([3.0, 4.0], 2, FLOAT64), EUCLIDEAN) AS distance"#,
        "{}",
    )?;
    require(
        typed["rows"]
            == serde_json::json!([["2024-07-01T12:00:00+02:00[Europe/Paris]", "值=3😀", 5.0]]),
        "temporal/vector/string-interpolation result mismatch",
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
