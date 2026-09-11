#![forbid(unsafe_code)]

use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, OwnerKind, PropertyValue, RelationshipRecord, allocate_node_id,
    allocate_relationship_id, branch_head, commit_layer, create_checkpoint, intern_label,
    intern_property_key, intern_relationship_type,
};
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

struct QueryFixture {
    fixture: FileDatabaseFixture,
    first_commit: String,
}

struct FixtureIds {
    person: i64,
    secret: i64,
    knows: i64,
    name: i64,
    age: i64,
    alice: i64,
    bob: i64,
    hidden: i64,
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
        .ok_or("usage: lithograph-phase04 <extension-path>")?;
    let path = fs::canonicalize(path)?;
    let load = extension_load_command(&path)?;
    let query_fixture = build_query_fixture(&load)?;
    let mut checks = Vec::new();

    check_scalar_rows_equivalence(&query_fixture.fixture, &load)?;
    checks.push("scalar-rows-same-encoding");
    check_historical_and_graph_view(&query_fixture, &load)?;
    checks.push("historical-graph-view");
    check_explain_profile(&query_fixture.fixture, &load)?;
    checks.push("explain-profile-adapters");
    check_invalid_graph_view_error(&query_fixture.fixture, &load)?;
    checks.push("invalid-graph-view-error");

    Ok(ProbeResult {
        phase: "04-read-query-engine",
        extension: path.display().to_string(),
        checks,
    })
}

fn build_query_fixture(load: &str) -> Result<QueryFixture, Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0401)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    let connection = Connection::open(fixture.path())?;
    let root = branch_head(&connection, "main")?;
    let ids = fixture_ids(&connection)?;
    let layer = fixture_layer(&connection, &ids)?;
    let first = commit_layer(
        &connection,
        "main",
        root,
        None,
        &layer,
        &metadata("phase04 adapter fixture", 1),
    )?;
    create_checkpoint(&connection, first)?;

    let mut second = LayerBuilder::default();
    second.set_property(
        OwnerKind::Node,
        ids.bob,
        ids.age,
        PropertyValue::Integer(34),
    )?;
    let _ = commit_layer(
        &connection,
        "main",
        first,
        None,
        &second,
        &metadata("phase04 current head", 2),
    )?;
    drop(connection);
    Ok(QueryFixture {
        fixture,
        first_commit: first.to_hex(),
    })
}

fn fixture_ids(connection: &Connection) -> Result<FixtureIds, Box<dyn Error>> {
    Ok(FixtureIds {
        person: intern_label(connection, "Person")?,
        secret: intern_label(connection, "Secret")?,
        knows: intern_relationship_type(connection, "KNOWS")?,
        name: intern_property_key(connection, "name")?,
        age: intern_property_key(connection, "age")?,
        alice: allocate_node_id(connection)?,
        bob: allocate_node_id(connection)?,
        hidden: allocate_node_id(connection)?,
    })
}

fn fixture_layer(
    connection: &Connection,
    ids: &FixtureIds,
) -> Result<LayerBuilder, Box<dyn Error>> {
    let mut layer = LayerBuilder::default();
    for node in [ids.alice, ids.bob, ids.hidden] {
        layer.add_node(node)?;
    }
    for node in [ids.alice, ids.bob] {
        layer.add_label(node, ids.person)?;
    }
    layer.add_label(ids.hidden, ids.secret)?;
    add_fixture_properties(&mut layer, ids)?;
    add_fixture_relationships(connection, &mut layer, ids)?;
    Ok(layer)
}

fn add_fixture_properties(
    layer: &mut LayerBuilder,
    ids: &FixtureIds,
) -> Result<(), Box<dyn Error>> {
    for (node, value, years) in [
        (ids.alice, "Alice", 41_i64),
        (ids.bob, "Bob", 33_i64),
        (ids.hidden, "Hidden", 99_i64),
    ] {
        layer.set_property(
            OwnerKind::Node,
            node,
            ids.name,
            PropertyValue::String(value.to_owned()),
        )?;
        layer.set_property(
            OwnerKind::Node,
            node,
            ids.age,
            PropertyValue::Integer(years),
        )?;
    }
    Ok(())
}

fn add_fixture_relationships(
    connection: &Connection,
    layer: &mut LayerBuilder,
    ids: &FixtureIds,
) -> Result<(), Box<dyn Error>> {
    for (source, target) in [(ids.alice, ids.bob), (ids.alice, ids.hidden)] {
        layer.add_relationship(RelationshipRecord {
            id: allocate_relationship_id(connection)?,
            source,
            type_id: ids.knows,
            target,
        })?;
    }
    Ok(())
}

fn check_scalar_rows_equivalence(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let query = "MATCH p=(a:Person)-[:KNOWS]->(b:Person) RETURN a, p ORDER BY a.name";
    let scalar = scalar_query(fixture, load, query, "{}")?;
    assert_scalar_path_shape(&scalar)?;
    let rows = scalar["rows"]
        .as_array()
        .ok_or("scalar rows must be an array")?;
    let lines = streamed_rows(fixture, load, query)?;
    require(
        lines.len() == rows.len(),
        "rows adapter row count must match scalar",
    )?;
    for (index, line) in lines.iter().enumerate() {
        assert_streamed_row(line, index, &scalar["columns"], &rows[index])?;
    }
    Ok(())
}

fn assert_scalar_path_shape(scalar: &Value) -> Result<(), Box<dyn Error>> {
    require(
        scalar["columns"] == serde_json::json!(["a", "p"]),
        "scalar columns must match projection",
    )?;
    let rows = scalar["rows"]
        .as_array()
        .ok_or("scalar rows must be an array")?;
    require(rows.len() == 1, "visible fixture must return one path row")?;
    require(
        rows[0][0]["$type"] == "Node" && rows[0][1]["$type"] == "Path",
        "scalar must preserve tagged Node and Path values",
    )?;
    require_read_summary(&scalar["summary"])?;
    Ok(())
}

fn streamed_rows(
    fixture: &FileDatabaseFixture,
    load: &str,
    query: &str,
) -> Result<Vec<String>, Box<dyn Error>> {
    let output = fixture.execute_script(&format!(
        "{load}\nSELECT ordinal || char(9) || columns || char(9) || row FROM lithograph_rows({}) ORDER BY ordinal;",
        sql_literal(query)
    ))?;
    Ok(output.lines().map(str::to_owned).collect())
}

fn assert_streamed_row(
    line: &str,
    index: usize,
    expected_columns: &Value,
    expected_row: &Value,
) -> Result<(), Box<dyn Error>> {
    let mut fields = line.splitn(3, '\t');
    let ordinal = fields.next().ok_or("missing rows ordinal")?;
    let columns = fields.next().ok_or("missing rows columns")?;
    let row = fields.next().ok_or("missing rows row")?;
    require(
        ordinal.parse::<usize>()? == index,
        "rows ordinal must be contiguous",
    )?;
    require(
        serde_json::from_str::<Value>(columns)? == *expected_columns,
        "rows columns must equal scalar columns",
    )?;
    require(
        serde_json::from_str::<Value>(row)? == *expected_row,
        "rows value encoding must equal scalar row encoding",
    )?;
    Ok(())
}

fn check_historical_and_graph_view(
    query_fixture: &QueryFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = &query_fixture.fixture;
    let bob_age = "MATCH (n:Person) WHERE n.name = 'Bob' RETURN n.age AS age";
    let current = scalar_query(fixture, load, bob_age, "{}")?;
    require(
        current["rows"] == serde_json::json!([[34]]),
        "current head age must be 34",
    )?;
    let at = format!(r#"{{"at":"commit/{}"}}"#, query_fixture.first_commit);
    let historical = scalar_query(fixture, load, bob_age, &at)?;
    require(
        historical["rows"] == serde_json::json!([[33]]),
        "historical commit must preserve the original age",
    )?;

    let traversal = "MATCH (a)-[:KNOWS]->(b) RETURN b.name AS name ORDER BY name";
    let full = scalar_query(fixture, load, traversal, "{}")?;
    require(
        full["rows"] == serde_json::json!([["Bob"], ["Hidden"]]),
        "full graph traversal must include the secret endpoint",
    )?;
    let view = r#"{"graphView":{"excludeAnyLabels":["Secret"]}}"#;
    let filtered = scalar_query(fixture, load, traversal, view)?;
    require(
        filtered["rows"] == serde_json::json!([["Bob"]]),
        "graphView must remove relationships with hidden endpoints",
    )?;
    Ok(())
}

fn check_explain_profile(fixture: &FileDatabaseFixture, load: &str) -> Result<(), Box<dyn Error>> {
    let query = "MATCH (a:Person)-[:KNOWS]->(b:Person) RETURN a.name AS name";
    let normal = scalar_query(fixture, load, query, "{}")?;
    let profile = scalar_query(fixture, load, &format!("PROFILE {query}"), "{}")?;
    require(
        profile["rows"] == normal["rows"] && profile["columns"] == normal["columns"],
        "PROFILE result must equal normal execution",
    )?;
    let explain = scalar_query(fixture, load, &format!("EXPLAIN {query}"), "{}")?;
    require(
        explain["columns"] == serde_json::json!(["plan"]),
        "EXPLAIN must return a plan column",
    )?;
    require(
        explain["rows"][0][0]
            .as_str()
            .is_some_and(|plan| plan.contains("AdjacencySeek")),
        "EXPLAIN must expose adjacency seek for typed expansion",
    )?;
    Ok(())
}

fn check_invalid_graph_view_error(
    fixture: &FileDatabaseFixture,
    load: &str,
) -> Result<(), Box<dyn Error>> {
    let script = format!(
        "{load}\nSELECT lithograph('MATCH (n) RETURN n', '{{}}', '{{\"graphView\":null}}');"
    );
    match fixture.execute_script(&script) {
        Err(FixtureError::Sqlite { stderr, .. }) => require(
            stderr.contains("INVALID_ARGUMENT"),
            "invalid graphView must use INVALID_ARGUMENT",
        ),
        Err(error) => Err(error.into()),
        Ok(_) => Err("invalid graphView unexpectedly succeeded".into()),
    }
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

fn require_read_summary(summary: &Value) -> Result<(), Box<dyn Error>> {
    require(
        summary["queryType"] == "read",
        "summary queryType must be read",
    )?;
    require(
        summary["commit"]
            .as_str()
            .is_some_and(|commit| commit.starts_with("commit/") && commit.len() == 71),
        "summary commit must be a commit descriptor",
    )?;
    for field in [
        "nodesCreated",
        "nodesDeleted",
        "relationshipsCreated",
        "relationshipsDeleted",
        "propertiesSet",
        "propertiesRemoved",
        "labelsAdded",
        "labelsRemoved",
        "constraintsAdded",
        "constraintsRemoved",
        "indexesAdded",
        "indexesRemoved",
    ] {
        require(
            summary["counters"][field].as_i64() == Some(0),
            "read counter must be present and zero",
        )?;
    }
    Ok(())
}

fn metadata(message: &str, committed_at: i64) -> CommitMetadata {
    CommitMetadata {
        author: Some("phase04-test".to_owned()),
        message: Some(message.to_owned()),
        committed_at,
    }
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
