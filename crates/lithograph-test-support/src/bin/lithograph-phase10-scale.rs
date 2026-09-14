use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use lithograph_core::cypher::Value;
use lithograph_core::query::{
    ExecutionOptions, QueryCursor, QuerySummary, SnapshotSelector, prepare,
};
use lithograph_core::storage::test_support::{ScaleFixture, ScaleFixtureSpec, seed_scale_fixture};
use lithograph_core::storage::{
    CommitMetadata, branch_head, clear_commit_data, commit_data, create_checkpoint,
    create_empty_commit, create_storage_schema, create_tag, initialize_connection_state,
    initialize_root, integrity_check, resolve_version_descriptor, root_commit, set_commit_data,
};
use rusqlite::Connection;
use serde::Serialize;

#[derive(Debug, Serialize)]
struct ScaleReport {
    profile: String,
    node_count: u64,
    relationship_count: u64,
    sample_document_count: u64,
    database_bytes: u64,
    machine: String,
    rustc: String,
    sqlite: String,
    fixture_commit: String,
    setup: Vec<Measurement>,
    workloads: Vec<Measurement>,
}

#[derive(Debug, Serialize)]
struct Measurement {
    name: String,
    elapsed_millis: u128,
    rows: u64,
    db_hits: u64,
    plan: Option<String>,
    detail: String,
}

struct ScaleConfig {
    node_count: u64,
    relationship_count: u64,
    sample_document_count: u64,
    hub_relationship_count: u64,
    progress_interval: u64,
    reuse: bool,
    skip_integrity: bool,
    profile: &'static str,
    root: PathBuf,
    database: PathBuf,
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = ScaleConfig::from_env()?;
    fs::create_dir_all(&config.root)?;
    let mut setup = Vec::new();
    let (connection, fixture) = open_scale_fixture(&config, &mut setup)?;
    verify_scale_integrity(&connection, &config, &mut setup)?;
    let workloads = run_scale_workloads(&connection, &config, fixture, &mut setup)?;
    emit_scale_report(&connection, &config, fixture, setup, workloads)
}

impl ScaleConfig {
    fn from_env() -> Result<Self, Box<dyn Error>> {
        let node_count = env_u64("LITHOGRAPH_SCALE_NODES", 10_000_000)?;
        let relationship_count = env_u64("LITHOGRAPH_SCALE_RELATIONSHIPS", 100_000_000)?;
        let root = PathBuf::from(
            std::env::var("LITHOGRAPH_SCALE_DIR")
                .unwrap_or_else(|_| "target/phase10-scale".to_owned()),
        );
        Ok(Self {
            node_count,
            relationship_count,
            sample_document_count: env_u64("LITHOGRAPH_SCALE_DOCUMENTS", 1_000)?,
            hub_relationship_count: env_u64("LITHOGRAPH_SCALE_HUB_RELATIONSHIPS", 1_000_000)?,
            progress_interval: env_u64("LITHOGRAPH_SCALE_PROGRESS", 1_000_000)?,
            reuse: env_bool("LITHOGRAPH_SCALE_REUSE")?,
            skip_integrity: env_bool("LITHOGRAPH_SCALE_SKIP_INTEGRITY")?,
            profile: if node_count == 10_000_000 && relationship_count == 100_000_000 {
                "release-10m-100m"
            } else {
                "custom"
            },
            database: root.join("scale.sqlite"),
            root,
        })
    }
}

fn open_scale_fixture(
    config: &ScaleConfig,
    setup: &mut Vec<Measurement>,
) -> Result<(Connection, ScaleFixture), Box<dyn Error>> {
    if config.reuse {
        let connection = Connection::open(&config.database)?;
        let fixture = existing_scale_fixture(
            &connection,
            config.node_count,
            config.relationship_count,
            config.hub_relationship_count,
        )?;
        setup.push(simple_measurement(
            "reuse_existing_fixture",
            Instant::now(),
            format!("commit/{}", fixture.commit.to_hex()),
        ));
        return Ok((connection, fixture));
    }
    create_scale_fixture(config, setup)
}

fn create_scale_fixture(
    config: &ScaleConfig,
    setup: &mut Vec<Measurement>,
) -> Result<(Connection, ScaleFixture), Box<dyn Error>> {
    remove_sqlite_files(&config.database)?;
    let connection = Connection::open(&config.database)?;
    initialize_scale_database(&connection)?;
    let started = Instant::now();
    let fixture = seed_scale_fixture(
        &connection,
        ScaleFixtureSpec {
            node_count: config.node_count,
            relationship_count: config.relationship_count,
            sample_document_count: config.sample_document_count,
            hub_relationship_count: config.hub_relationship_count,
            progress_interval: config.progress_interval,
        },
        |phase, current, total| eprintln!("phase10-scale {phase}: {current}/{total}"),
    )?;
    setup.push(simple_measurement(
        "canonical_fixture_seed",
        started,
        format!("commit/{}", fixture.commit.to_hex()),
    ));
    let started = Instant::now();
    create_checkpoint(&connection, fixture.commit)?;
    setup.push(simple_measurement(
        "checkpoint_rebuild",
        started,
        "set-based first-parent Layer replay".to_owned(),
    ));
    Ok((connection, fixture))
}

fn initialize_scale_database(connection: &Connection) -> Result<(), Box<dyn Error>> {
    connection.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=FILE; PRAGMA cache_size=-65536;\
         CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1),\
             magic TEXT NOT NULL,\
             database_id TEXT NOT NULL,\
             storage_format INTEGER NOT NULL\
         );\
         INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format)\
         VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000000010', 2);",
    )?;
    create_storage_schema(connection)?;
    initialize_root(connection)?;
    initialize_connection_state(connection)?;
    Ok(())
}

fn verify_scale_integrity(
    connection: &Connection,
    config: &ScaleConfig,
    setup: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    if config.skip_integrity {
        return Ok(());
    }
    let started = Instant::now();
    let issues = integrity_check(connection)?;
    if !issues.is_empty() {
        return Err(format!("scale fixture integrity failed: {issues:?}").into());
    }
    setup.push(simple_measurement(
        "integrity_check",
        started,
        "0 issues".to_owned(),
    ));
    Ok(())
}

fn run_scale_workloads(
    connection: &Connection,
    config: &ScaleConfig,
    fixture: ScaleFixture,
    setup: &mut Vec<Measurement>,
) -> Result<Vec<Measurement>, Box<dyn Error>> {
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL")?;
    let mut workloads = Vec::new();
    install_scale_schema(connection, setup)?;
    run_read_workloads(
        connection,
        fixture,
        config.node_count,
        config.relationship_count,
        config.sample_document_count.min(config.node_count),
        &mut workloads,
    )?;
    run_version_workloads(connection, fixture, &mut workloads)?;
    run_write_workload(connection, &mut workloads)?;
    Ok(workloads)
}

fn emit_scale_report(
    connection: &Connection,
    config: &ScaleConfig,
    fixture: ScaleFixture,
    setup: Vec<Measurement>,
    workloads: Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    let report = ScaleReport {
        profile: config.profile.to_owned(),
        node_count: config.node_count,
        relationship_count: config.relationship_count,
        sample_document_count: config.sample_document_count,
        database_bytes: sqlite_files_size(&config.database)?,
        machine: command_output("uname", &["-a"]),
        rustc: command_output("rustc", &["--version"]),
        sqlite: connection.query_row("SELECT sqlite_version()", [], |row| row.get(0))?,
        fixture_commit: format!("commit/{}", fixture.commit.to_hex()),
        setup,
        workloads,
    };
    let report_path = config.root.join("baseline.json");
    fs::write(&report_path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("phase10-scale report: {}", report_path.display());
    Ok(())
}

fn install_scale_schema(
    connection: &Connection,
    measurements: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    for (name, query) in [
        (
            "property_type_constraint",
            "CREATE CONSTRAINT scale_id_type FOR (n:ScaleNode) REQUIRE n.scaleId IS :: INTEGER",
        ),
        (
            "fulltext_index",
            "CREATE FULLTEXT INDEX scale_text FOR (n:ScaleDocument) ON EACH [n.text]",
        ),
        (
            "vector_index",
            "CREATE VECTOR INDEX scale_embedding FOR (n:ScaleDocument) ON (n.embedding) OPTIONS {indexConfig:{`vector.dimensions`:2, `vector.similarity_function`:'cosine'}}",
        ),
        (
            "range_index",
            "CREATE RANGE INDEX scale_id FOR (n:ScaleNode) ON (n.scaleId)",
        ),
    ] {
        let started = Instant::now();
        let (_, summary) = execute(
            connection,
            query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )?;
        measurements.push(query_measurement(
            name,
            started,
            &summary,
            None,
            "schema ready",
        ));
    }
    Ok(())
}

fn run_read_workloads(
    connection: &Connection,
    fixture: ScaleFixture,
    node_count: u64,
    relationship_count: u64,
    sample_document_count: u64,
    workloads: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    measure_streamed_rows(
        connection,
        workloads,
        "label_scan",
        "MATCH (n:ScaleNode) RETURN n.scaleId",
        BTreeMap::new(),
        node_count,
        &["LabelIndexScan"],
    )?;

    let target = i64::try_from(node_count / 2 + 1)?;
    measure_rows(
        connection,
        workloads,
        "indexed_equality_seek",
        "MATCH (n:ScaleNode) WHERE n.scaleId = $target RETURN n.scaleId",
        params(&[("target", target)]),
        1,
        Some("IndexSeek"),
    )?;

    let lower = i64::try_from(node_count / 2)?;
    let width = 1_000_u64.min(node_count.saturating_sub(node_count / 2));
    measure_rows(
        connection,
        workloads,
        "indexed_range_seek",
        "MATCH (n:ScaleNode) WHERE n.scaleId >= $lower AND n.scaleId < $upper RETURN n.scaleId",
        params(&[("lower", lower), ("upper", lower + i64::try_from(width)?)]),
        width,
        Some("IndexSeek"),
    )?;

    measure_streamed_rows(
        connection,
        workloads,
        "low_degree_one_hop",
        "MATCH (:ScaleLowDegree)-[:SCALE_LINK]->(m) RETURN 1",
        BTreeMap::new(),
        u64::from(relationship_count >= 2),
        &["LabelIndexScan", "AdjacencySeek"],
    )?;

    measure_streamed_rows(
        connection,
        workloads,
        "high_degree_one_hop",
        "MATCH (:ScaleHub)-[:SCALE_LINK]->(m) RETURN 1",
        BTreeMap::new(),
        fixture.hub_outgoing_count,
        &["LabelIndexScan", "AdjacencySeek"],
    )?;

    if node_count >= 6 && relationship_count >= 5 {
        measure_streamed_rows(
            connection,
            workloads,
            "variable_path",
            "MATCH (:ScaleLowDegree)-[:SCALE_LINK*1..4]->(m) RETURN 1",
            BTreeMap::new(),
            4,
            &[],
        )?;
    }

    measure_count(
        connection,
        workloads,
        "fulltext",
        "CALL db.index.fulltext.queryNodes('scale_text', 'graph') YIELD node RETURN count(node)",
        BTreeMap::new(),
        sample_document_count,
        None,
    )?;

    let started = Instant::now();
    let (rows, summary) = execute(
        connection,
        "MATCH (n:ScaleDocument) SEARCH n IN (VECTOR INDEX scale_embedding FOR vector([1.0,0.0],2,FLOAT64) LIMIT 10) SCORE AS score RETURN n.scaleId, score",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )?;
    if rows.is_empty() || rows.len() > 10 {
        return Err("vector SEARCH returned an invalid top-k cardinality".into());
    }
    workloads.push(query_measurement(
        "vector_search",
        started,
        &summary,
        None,
        &format!("{} result rows", rows.len()),
    ));

    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(fixture.commit.to_hex());
    measure_streamed_rows_with_options(
        connection,
        workloads,
        "historical_query",
        "MATCH (n:ScaleHub) RETURN n.scaleId",
        BTreeMap::new(),
        1,
        historical,
        &["LabelIndexScan"],
    )?;
    Ok(())
}

fn run_version_workloads(
    connection: &Connection,
    fixture: ScaleFixture,
    workloads: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    measure_tag_lookup(connection, fixture, workloads)?;
    measure_commit_data(connection, fixture, workloads)?;
    measure_branch_diff(connection, workloads)?;
    measure_cursor_history(connection, workloads)
}

fn measure_tag_lookup(
    connection: &Connection,
    fixture: ScaleFixture,
    workloads: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    create_tag(connection, "phase10-scale", fixture.commit)?;
    let resolved = resolve_version_descriptor(connection, "tag/phase10-scale")?;
    if resolved != fixture.commit {
        return Err("Tag lookup did not preserve the fixture Commit".into());
    }
    workloads.push(simple_measurement(
        "tag_lookup_gc_root",
        started,
        "tag resolves to pinned fixture Commit".to_owned(),
    ));
    Ok(())
}

fn measure_commit_data(
    connection: &Connection,
    fixture: ScaleFixture,
    workloads: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    set_commit_data(connection, fixture.commit, r#"{"phase":10,"scale":true}"#)?;
    if commit_data(connection, fixture.commit)?.is_none() {
        return Err("Commit Data set/get failed".into());
    }
    clear_commit_data(connection, fixture.commit)?;
    if commit_data(connection, fixture.commit)?.is_some() {
        return Err("Commit Data clear failed".into());
    }
    workloads.push(simple_measurement(
        "commit_data_get_set_clear",
        started,
        "round-trip + clear".to_owned(),
    ));
    Ok(())
}

fn measure_branch_diff(
    connection: &Connection,
    workloads: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    let baseline = branch_head(connection, "main")?;
    create_tag(connection, "phase10-diff-base", baseline)?;
    execute(
        connection,
        "UNWIND range(1,100) AS i CREATE (:ScaleDiff {value:i}) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )?;
    let started = Instant::now();
    let (rows, summary) = execute(
        connection,
        "CALL lithograph.diff('tag/phase10-diff-base', 'branch/main') YIELD patch RETURN patch",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )?;
    let operation_count = rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Map(patch) => patch.get("operations"),
            _ => None,
        })
        .and_then(|value| match value {
            Value::List(operations) => Some(operations.len()),
            _ => None,
        })
        .ok_or("branch diff did not return a structured Patch")?;
    if operation_count < 100 {
        return Err("branch diff did not report the small branch delta".into());
    }
    workloads.push(query_measurement(
        "branch_diff",
        started,
        &summary,
        None,
        &format!("{operation_count} operations over large base graph"),
    ));
    Ok(())
}

fn measure_cursor_history(
    connection: &Connection,
    workloads: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    let mut head = branch_head(connection, "main")?;
    for index in 0..256_i64 {
        head = create_empty_commit(
            connection,
            "main",
            head,
            &CommitMetadata {
                author: Some("phase10-scale".to_owned()),
                message: Some(format!("history-{index}")),
                committed_at: 10_000 + index,
            },
        )?;
    }
    let started = Instant::now();
    let mut cursor: Option<String> = None;
    let mut seen = 0_u64;
    for _ in 0..4 {
        let query = match cursor.as_deref() {
            Some(cursor) => format!(
                "CALL lithograph.log('branch/main', 64, '{cursor}') YIELD commit, cursor RETURN commit, cursor"
            ),
            None => {
                "CALL lithograph.log('branch/main', 64) YIELD commit, cursor RETURN commit, cursor"
                    .to_owned()
            }
        };
        let (rows, _) = execute(
            connection,
            &query,
            BTreeMap::new(),
            ExecutionOptions::default(),
        )?;
        seen += rows.len() as u64;
        cursor = rows
            .last()
            .and_then(|row| row.get(1))
            .and_then(|value| match value {
                Value::String(value) => Some(value.clone()),
                _ => None,
            });
    }
    if seen != 256 {
        return Err(format!("cursor history returned {seen}, expected 256").into());
    }
    workloads.push(simple_measurement(
        "cursor_commit_dag_history",
        started,
        "4 pages x 64 commits".to_owned(),
    ));
    Ok(())
}

fn run_write_workload(
    connection: &Connection,
    workloads: &mut Vec<Measurement>,
) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let (_, summary) = execute(
        connection,
        "UNWIND range(1,1000) AS i CREATE (:ScaleWrite {value:i}) FINISH",
        BTreeMap::new(),
        ExecutionOptions::default(),
    )?;
    if summary.counters.nodes_created != 1_000 {
        return Err("write batch did not create exactly 1000 Nodes".into());
    }
    workloads.push(query_measurement(
        "write_batch_commit",
        started,
        &summary,
        None,
        "1000 Node mutation batch / one Commit",
    ));
    Ok(())
}

fn measure_count(
    connection: &Connection,
    measurements: &mut Vec<Measurement>,
    name: &str,
    query: &str,
    parameters: BTreeMap<String, Value>,
    expected: u64,
    expected_plan: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    measure_count_with_options(
        connection,
        measurements,
        name,
        query,
        parameters,
        expected,
        ExecutionOptions::default(),
        expected_plan,
    )
}

fn measure_rows(
    connection: &Connection,
    measurements: &mut Vec<Measurement>,
    name: &str,
    query: &str,
    parameters: BTreeMap<String, Value>,
    expected: u64,
    expected_plan: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let plan = if let Some(operator) = expected_plan {
        let (rows, _) = execute(
            connection,
            &format!("EXPLAIN {query}"),
            parameters.clone(),
            ExecutionOptions::default(),
        )?;
        let plan = format!("{rows:?}");
        if !plan.contains(operator) {
            return Err(format!("{name} plan did not contain {operator}: {plan}").into());
        }
        Some(plan)
    } else {
        None
    };
    let started = Instant::now();
    let (rows, summary) = execute(connection, query, parameters, ExecutionOptions::default())?;
    let actual = rows.len() as u64;
    if actual != expected {
        return Err(format!("{name} returned {actual} rows, expected {expected}").into());
    }
    measurements.push(query_measurement(
        name,
        started,
        &summary,
        plan,
        &format!("rows={actual}"),
    ));
    Ok(())
}

fn measure_streamed_rows(
    connection: &Connection,
    measurements: &mut Vec<Measurement>,
    name: &str,
    query: &str,
    parameters: BTreeMap<String, Value>,
    expected: u64,
    expected_plan: &[&str],
) -> Result<(), Box<dyn Error>> {
    measure_streamed_rows_with_options(
        connection,
        measurements,
        name,
        query,
        parameters,
        expected,
        ExecutionOptions::default(),
        expected_plan,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the scale measurement helper keeps query inputs, expected cardinality, options, and plan gate explicit"
)]
fn measure_streamed_rows_with_options(
    connection: &Connection,
    measurements: &mut Vec<Measurement>,
    name: &str,
    query: &str,
    parameters: BTreeMap<String, Value>,
    expected: u64,
    options: ExecutionOptions,
    expected_plan: &[&str],
) -> Result<(), Box<dyn Error>> {
    let plan = if expected_plan.is_empty() {
        None
    } else {
        let (rows, _) = execute(
            connection,
            &format!("EXPLAIN {query}"),
            parameters.clone(),
            options.clone(),
        )?;
        let plan = format!("{rows:?}");
        for operator in expected_plan {
            if !plan.contains(operator) {
                return Err(format!("{name} plan did not contain {operator}: {plan}").into());
            }
        }
        Some(plan)
    };

    let prepared = prepare(connection, query, parameters, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let started = Instant::now();
    let mut actual = 0_u64;
    loop {
        let batch = cursor.next_batch(connection, 256)?;
        actual = actual.saturating_add(batch.rows.len() as u64);
        if batch.done {
            let summary = cursor.complete(connection)?;
            if actual != expected {
                return Err(format!("{name} streamed {actual} rows, expected {expected}").into());
            }
            measurements.push(query_measurement(
                name,
                started,
                &summary,
                plan,
                &format!("streamedRows={actual}"),
            ));
            return Ok(());
        }
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the scale count helper keeps query inputs, expected cardinality, options, and plan gate explicit"
)]
fn measure_count_with_options(
    connection: &Connection,
    measurements: &mut Vec<Measurement>,
    name: &str,
    query: &str,
    parameters: BTreeMap<String, Value>,
    expected: u64,
    options: ExecutionOptions,
    expected_plan: Option<&str>,
) -> Result<(), Box<dyn Error>> {
    let plan = if let Some(operator) = expected_plan {
        let (rows, _) = execute(
            connection,
            &format!("EXPLAIN {query}"),
            parameters.clone(),
            options.clone(),
        )?;
        let plan = format!("{rows:?}");
        if !plan.contains(operator) {
            return Err(format!("{name} plan did not contain {operator}: {plan}").into());
        }
        Some(plan)
    } else {
        None
    };
    let started = Instant::now();
    let (rows, summary) = execute(connection, query, parameters, options)?;
    let count = rows
        .first()
        .and_then(|row| row.first())
        .and_then(|value| match value {
            Value::Integer(value) => u64::try_from(*value).ok(),
            _ => None,
        })
        .ok_or_else(|| format!("{name} did not return one non-negative Integer count"))?;
    if count != expected {
        return Err(format!("{name} returned {count}, expected {expected}").into());
    }
    measurements.push(query_measurement(
        name,
        started,
        &summary,
        plan,
        &format!("count={count}"),
    ));
    Ok(())
}

fn execute(
    connection: &Connection,
    query: &str,
    parameters: BTreeMap<String, Value>,
    options: ExecutionOptions,
) -> Result<(Vec<Vec<Value>>, QuerySummary), lithograph_core::query::QueryError> {
    let prepared = prepare(connection, query, parameters, options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 256)?;
        rows.extend(batch.rows);
        if batch.done {
            return Ok((rows, cursor.complete(connection)?));
        }
    }
}

fn params(values: &[(&str, i64)]) -> BTreeMap<String, Value> {
    values
        .iter()
        .map(|(name, value)| ((*name).to_owned(), Value::Integer(*value)))
        .collect()
}

fn query_measurement(
    name: &str,
    started: Instant,
    summary: &QuerySummary,
    plan: Option<String>,
    detail: &str,
) -> Measurement {
    Measurement {
        name: name.to_owned(),
        elapsed_millis: started.elapsed().as_millis(),
        rows: summary.metrics.rows,
        db_hits: summary.metrics.db_hits,
        plan,
        detail: detail.to_owned(),
    }
}

fn simple_measurement(name: &str, started: Instant, detail: String) -> Measurement {
    Measurement {
        name: name.to_owned(),
        elapsed_millis: started.elapsed().as_millis(),
        rows: 0,
        db_hits: 0,
        plan: None,
        detail,
    }
}

fn env_u64(name: &str, default: u64) -> Result<u64, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse::<u64>()?),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn env_bool(name: &str) -> Result<bool, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => match value.as_str() {
            "1" | "true" | "TRUE" | "yes" | "YES" => Ok(true),
            "0" | "false" | "FALSE" | "no" | "NO" => Ok(false),
            _ => Err(format!("{name} must be a boolean flag").into()),
        },
        Err(std::env::VarError::NotPresent) => Ok(false),
        Err(error) => Err(error.into()),
    }
}

fn existing_scale_fixture(
    connection: &Connection,
    node_count: u64,
    relationship_count: u64,
    requested_hub_relationships: u64,
) -> Result<ScaleFixture, Box<dyn Error>> {
    let commit = branch_head(connection, "main")?;
    let root = root_commit(connection)?;
    let commit_bytes = commit.as_bytes().as_slice();
    let (first_node, first_relationship) =
        verify_existing_dimensions(connection, commit_bytes, node_count, relationship_count)?;
    let scale_label = dictionary_id(connection, "_lithograph_labels", "ScaleNode")?;
    let document_label = dictionary_id(connection, "_lithograph_labels", "ScaleDocument")?;
    let low_degree_label = dictionary_id(connection, "_lithograph_labels", "ScaleLowDegree")?;
    let hub_label = dictionary_id(connection, "_lithograph_labels", "ScaleHub")?;
    let link_type = dictionary_id(connection, "_lithograph_rel_types", "SCALE_LINK")?;
    let hub_outgoing_count = existing_hub_outgoing_count(
        connection,
        commit_bytes,
        hub_label,
        link_type,
        requested_hub_relationships,
    )?;
    Ok(ScaleFixture {
        root,
        commit,
        first_node,
        first_relationship,
        scale_label,
        document_label,
        low_degree_label,
        hub_label,
        link_type,
        hub_outgoing_count,
    })
}

fn verify_existing_dimensions(
    connection: &Connection,
    commit_bytes: &[u8],
    node_count: u64,
    relationship_count: u64,
) -> Result<(i64, i64), Box<dyn Error>> {
    let (first_node, last_node) =
        checkpoint_id_bounds(connection, "node_id", "_lithograph_cp_nodes", commit_bytes)?;
    let (first_relationship, last_relationship) = checkpoint_id_bounds(
        connection,
        "relationship_id",
        "_lithograph_cp_relationships",
        commit_bytes,
    )?;
    let checkpoint_nodes = inclusive_span(first_node, last_node)?;
    let checkpoint_relationships = inclusive_span(first_relationship, last_relationship)?;
    if checkpoint_nodes != node_count || checkpoint_relationships != relationship_count {
        return Err(format!(
            "existing scale fixture dimensions differ: nodes={checkpoint_nodes}, relationships={checkpoint_relationships}"
        )
        .into());
    }
    Ok((first_node, first_relationship))
}

fn dictionary_id(connection: &Connection, table: &str, name: &str) -> Result<i64, Box<dyn Error>> {
    let sql = format!("SELECT id FROM main.{table} WHERE name = ?1");
    Ok(connection.query_row(&sql, [name], |row| row.get(0))?)
}

fn checkpoint_id_bounds(
    connection: &Connection,
    column: &str,
    table: &str,
    commit_bytes: &[u8],
) -> Result<(i64, i64), Box<dyn Error>> {
    let first_sql = format!(
        "SELECT {column} FROM main.{table} WHERE commit_id = ?1 ORDER BY {column} ASC LIMIT 1"
    );
    let last_sql = format!(
        "SELECT {column} FROM main.{table} WHERE commit_id = ?1 ORDER BY {column} DESC LIMIT 1"
    );
    let first = connection.query_row(&first_sql, [commit_bytes], |row| row.get(0))?;
    let last = connection.query_row(&last_sql, [commit_bytes], |row| row.get(0))?;
    Ok((first, last))
}

fn inclusive_span(first: i64, last: i64) -> Result<u64, Box<dyn Error>> {
    let span = last
        .checked_sub(first)
        .and_then(|value| value.checked_add(1))
        .ok_or("invalid checkpoint identity bounds")?;
    Ok(u64::try_from(span)?)
}

fn existing_hub_outgoing_count(
    connection: &Connection,
    commit_bytes: &[u8],
    hub_label: i64,
    link_type: i64,
    requested_hub_relationships: u64,
) -> Result<u64, Box<dyn Error>> {
    let hub_node: i64 = connection.query_row(
        "SELECT node_id FROM main._lithograph_cp_labels WHERE commit_id = ?1 AND label_id = ?2 LIMIT 1",
        rusqlite::params![commit_bytes, hub_label],
        |row| row.get(0),
    )?;
    let hub_outgoing_count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_cp_relationships WHERE commit_id = ?1 AND source_id = ?2 AND type_id = ?3",
        rusqlite::params![commit_bytes, hub_node, link_type],
        |row| row.get(0),
    )?;
    let hub_outgoing_count = u64::try_from(hub_outgoing_count)?;
    if hub_outgoing_count > requested_hub_relationships {
        return Err(format!(
            "existing scale fixture hub degree {hub_outgoing_count} exceeds configured {requested_hub_relationships}"
        )
        .into());
    }
    Ok(hub_outgoing_count)
}

fn remove_sqlite_files(path: &Path) -> Result<(), Box<dyn Error>> {
    for path in [
        path.to_path_buf(),
        PathBuf::from(format!("{}-wal", path.display())),
        PathBuf::from(format!("{}-shm", path.display())),
    ] {
        if path.exists() {
            fs::remove_file(path)?;
        }
    }
    Ok(())
}

fn sqlite_files_size(path: &Path) -> Result<u64, Box<dyn Error>> {
    let mut size = fs::metadata(path)?.len();
    for suffix in ["-wal", "-shm"] {
        let extra = PathBuf::from(format!("{}{suffix}", path.display()));
        if extra.exists() {
            size = size.saturating_add(fs::metadata(extra)?.len());
        }
    }
    Ok(size)
}

fn command_output(command: &str, args: &[&str]) -> String {
    Command::new(command)
        .args(args)
        .output()
        .ok()
        .filter(|output| output.status.success())
        .map(|output| String::from_utf8_lossy(&output.stdout).trim().to_owned())
        .unwrap_or_else(|| "unavailable".to_owned())
}
