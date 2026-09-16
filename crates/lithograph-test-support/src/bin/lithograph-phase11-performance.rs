use std::collections::BTreeMap;
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use lithograph_core::cypher::Value;
use lithograph_core::performance::{self, PerformanceCounters};
use lithograph_core::query::{ExecutionOptions, QueryCursor, prepare};
use lithograph_core::storage::{HashId, Snapshot, branch_head};
use rusqlite::{Connection, StatementStatus, params};
use serde::Serialize;

const DEFAULT_DATABASE: &str = "target/phase10-scale-release/scale.sqlite";
const DEFAULT_OUTPUT_DIR: &str = "target/phase11-performance";
const DEFAULT_CASES: &str = "low_degree_one_hop,indexed_equality";

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PerformanceReport {
    schema_version: u32,
    phase: String,
    provenance: Provenance,
    fixture: FixtureManifest,
    sqlite: SqliteEnvironment,
    instrumentation: InstrumentationMeasurement,
    cases: Vec<CaseReport>,
    failures: Vec<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Provenance {
    git_commit: String,
    dirty: bool,
    dirty_diff_blake3: String,
    rustc: String,
    os: String,
    architecture: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FixtureManifest {
    path: String,
    database_bytes: u64,
    checkpoint_commit: String,
    query_commit: String,
    node_count: u64,
    relationship_count: u64,
    scale_node_count: u64,
    hub_degree: u64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SqliteEnvironment {
    version: String,
    compile_options: Vec<String>,
    journal_mode: String,
    synchronous: i64,
    cache_size: i64,
    temp_store: i64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct InstrumentationMeasurement {
    iterations: u64,
    disabled_micros: u128,
    enabled_micros: u128,
    overhead_micros: i128,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct CaseReport {
    name: String,
    adapter: String,
    cache_state: String,
    query: String,
    parameters: BTreeMap<String, String>,
    sample_count: usize,
    expected_rows: u64,
    samples: Vec<QuerySample>,
    explain: String,
    physical_probe: Option<PhysicalProbe>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct QuerySample {
    total_micros: u128,
    prepare_micros: u128,
    consume_micros: u128,
    first_row_micros: Option<u128>,
    rows: u64,
    checksum_blake3: String,
    db_hits: u64,
    counters: PerformanceCountersReport,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PerformanceCountersReport {
    resolved_state_builds: u64,
    lineage_layers_loaded: u64,
    standard_index_builds: u64,
    changed_owners: u64,
    standard_index_scan_micros: u64,
    adjacency_pages: u64,
}

impl From<PerformanceCounters> for PerformanceCountersReport {
    fn from(value: PerformanceCounters) -> Self {
        Self {
            resolved_state_builds: value.resolved_state_builds,
            lineage_layers_loaded: value.lineage_layers_loaded,
            standard_index_builds: value.standard_index_builds,
            changed_owners: value.changed_owners,
            standard_index_scan_micros: value.standard_index_scan_micros,
            adjacency_pages: value.adjacency_pages,
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct PhysicalProbe {
    sql: String,
    query_plan: Vec<String>,
    vm_steps: i32,
    fullscan_steps: i32,
    sorts: i32,
    rows: u64,
}

#[derive(Clone)]
struct CaseSpec {
    name: &'static str,
    query: &'static str,
    parameters: BTreeMap<String, Value>,
    report_parameters: BTreeMap<String, String>,
    expected_rows: u64,
}

struct Config {
    database: PathBuf,
    output_dir: PathBuf,
    report_name: String,
    cases: Vec<String>,
    samples: usize,
    batch_size: usize,
    enforce_gates: bool,
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    fs::create_dir_all(&config.output_dir)?;
    let connection = Connection::open(&config.database)?;
    let fixture = inspect_fixture(&connection, &config.database)?;
    let sqlite = inspect_sqlite(&connection)?;
    let provenance = inspect_provenance()?;
    let instrumentation =
        measure_instrumentation(&connection, parse_commit(&fixture.query_commit)?)?;
    let specs = selected_cases(&connection, &fixture, &config.cases)?;
    let mut failures = Vec::new();
    let mut cases = Vec::new();
    for spec in specs {
        match run_case(&connection, &config, &fixture, &spec) {
            Ok(report) => cases.push(report),
            Err(error) => failures.push(format!("{}: {error}", spec.name)),
        }
    }
    performance::set_enabled(false);
    let report = PerformanceReport {
        schema_version: 1,
        phase: format!("phase11-{}", config.report_name),
        provenance,
        fixture,
        sqlite,
        instrumentation,
        cases,
        failures,
    };
    let path = config
        .output_dir
        .join(format!("{}.json", config.report_name));
    fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("phase11 performance report: {}", path.display());
    if report.failures.is_empty() {
        Ok(())
    } else {
        Err("one or more Phase 11 performance cases failed".into())
    }
}

impl Config {
    fn from_env() -> Result<Self, Box<dyn Error>> {
        let cases = std::env::var("LITHOGRAPH_PHASE11_CASES")
            .unwrap_or_else(|_| DEFAULT_CASES.to_owned())
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect::<Vec<_>>();
        if cases.is_empty() {
            return Err("LITHOGRAPH_PHASE11_CASES must select at least one case".into());
        }
        Ok(Self {
            database: PathBuf::from(
                std::env::var("LITHOGRAPH_PHASE11_DATABASE")
                    .unwrap_or_else(|_| DEFAULT_DATABASE.to_owned()),
            ),
            output_dir: PathBuf::from(
                std::env::var("LITHOGRAPH_PHASE11_DIR")
                    .unwrap_or_else(|_| DEFAULT_OUTPUT_DIR.to_owned()),
            ),
            report_name: env_report_name()?,
            cases,
            samples: env_usize("LITHOGRAPH_PHASE11_SAMPLES", 1)?.max(1),
            batch_size: env_usize("LITHOGRAPH_PHASE11_BATCH_SIZE", 256)?.clamp(1, 4_096),
            enforce_gates: env_bool("LITHOGRAPH_PHASE11_ENFORCE_GATES")?,
        })
    }
}

fn env_report_name() -> Result<String, Box<dyn Error>> {
    let value = std::env::var("LITHOGRAPH_PHASE11_REPORT").unwrap_or_else(|_| "after".to_owned());
    if value.is_empty() || !value.bytes().all(report_name_byte_is_safe) {
        return Err("LITHOGRAPH_PHASE11_REPORT must be a non-empty filename-safe token".into());
    }
    Ok(value)
}

fn report_name_byte_is_safe(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == 45 || byte == 95
}

fn inspect_fixture(
    connection: &Connection,
    path: &Path,
) -> Result<FixtureManifest, Box<dyn Error>> {
    let query_commit = branch_head(connection, "main")?;
    let checkpoint_commit = scale_checkpoint_commit(connection)?;
    let statistics = checkpoint_statistics(connection, checkpoint_commit)?;
    let node_count = statistic_count(&statistics, "nodes")?;
    let relationship_count = statistic_count(&statistics, "relationships")?;
    let scale_node_label = label_id(connection, "ScaleNode")?;
    let scale_node_count = grouped_statistic_count(&statistics, "labels", scale_node_label)?;
    let hub_node = checkpoint_label_node(connection, checkpoint_commit, "ScaleHub")?;
    let scale_link_type = relationship_type_id(connection, "SCALE_LINK")?;
    let hub_degree =
        checkpoint_outgoing_count(connection, checkpoint_commit, hub_node, scale_link_type)?;
    Ok(FixtureManifest {
        path: path.display().to_string(),
        database_bytes: fs::metadata(path)?.len(),
        checkpoint_commit: format!("commit/{}", checkpoint_commit.to_hex()),
        query_commit: format!("commit/{}", query_commit.to_hex()),
        node_count,
        relationship_count,
        scale_node_count,
        hub_degree,
    })
}

fn checkpoint_statistics(
    connection: &Connection,
    commit: HashId,
) -> Result<serde_json::Value, Box<dyn Error>> {
    let metadata: Vec<u8> = connection.query_row(
        "SELECT metadata FROM main._lithograph_checkpoints WHERE commit_id = ?1",
        [commit.as_bytes().as_slice()],
        |row| row.get(0),
    )?;
    Ok(serde_json::from_slice(&metadata)?)
}

fn statistic_count(statistics: &serde_json::Value, name: &str) -> Result<u64, Box<dyn Error>> {
    statistics
        .get(name)
        .and_then(serde_json::Value::as_u64)
        .ok_or_else(|| format!("checkpoint statistics are missing {name}").into())
}

fn grouped_statistic_count(
    statistics: &serde_json::Value,
    name: &str,
    id: i64,
) -> Result<u64, Box<dyn Error>> {
    let entries = statistics
        .get(name)
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| -> Box<dyn Error> {
            format!("checkpoint statistics are missing {name}").into()
        })?;
    entries
        .iter()
        .find_map(|entry| {
            let pair = entry.as_array()?;
            (pair.first()?.as_i64()? == id)
                .then(|| pair.get(1)?.as_u64())
                .flatten()
        })
        .ok_or_else(|| -> Box<dyn Error> {
            format!("checkpoint statistics are missing {name} id {id}").into()
        })
}

fn label_id(connection: &Connection, name: &str) -> Result<i64, Box<dyn Error>> {
    Ok(connection.query_row(
        "SELECT id FROM main._lithograph_labels WHERE name = ?1",
        [name],
        |row| row.get(0),
    )?)
}

fn relationship_type_id(connection: &Connection, name: &str) -> Result<i64, Box<dyn Error>> {
    Ok(connection.query_row(
        "SELECT id FROM main._lithograph_rel_types WHERE name = ?1",
        [name],
        |row| row.get(0),
    )?)
}

fn checkpoint_label_node(
    connection: &Connection,
    commit: HashId,
    label: &str,
) -> Result<i64, Box<dyn Error>> {
    let label_id = label_id(connection, label)?;
    Ok(connection.query_row(
        "SELECT node_id FROM main._lithograph_cp_labels INDEXED BY _lithograph_cp_labels_by_label \
         WHERE commit_id = ?1 AND label_id = ?2 ORDER BY node_id LIMIT 1",
        params![commit.as_bytes().as_slice(), label_id],
        |row| row.get(0),
    )?)
}

fn checkpoint_outgoing_count(
    connection: &Connection,
    commit: HashId,
    source_id: i64,
    type_id: i64,
) -> Result<u64, Box<dyn Error>> {
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_cp_relationships INDEXED BY _lithograph_cp_rel_out \
         WHERE commit_id = ?1 AND source_id = ?2 AND type_id = ?3",
        params![commit.as_bytes().as_slice(), source_id, type_id],
        |row| row.get(0),
    )?;
    Ok(u64::try_from(count)?)
}

fn scale_checkpoint_commit(connection: &Connection) -> Result<HashId, Box<dyn Error>> {
    let bytes: Vec<u8> = connection.query_row(
        "SELECT labels.commit_id \
         FROM main._lithograph_cp_labels AS labels \
         JOIN main._lithograph_labels AS dictionary ON dictionary.id = labels.label_id \
         WHERE dictionary.name = 'ScaleNode' \
         GROUP BY labels.commit_id ORDER BY count(*) DESC LIMIT 1",
        [],
        |row| row.get(0),
    )?;
    Ok(HashId::from_slice(&bytes)?)
}

fn inspect_sqlite(connection: &Connection) -> Result<SqliteEnvironment, Box<dyn Error>> {
    let compile_options = connection
        .prepare("PRAGMA compile_options")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(SqliteEnvironment {
        version: connection.query_row("SELECT sqlite_version()", [], |row| row.get(0))?,
        compile_options,
        journal_mode: connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?,
        synchronous: connection.query_row("PRAGMA synchronous", [], |row| row.get(0))?,
        cache_size: connection.query_row("PRAGMA cache_size", [], |row| row.get(0))?,
        temp_store: connection.query_row("PRAGMA temp_store", [], |row| row.get(0))?,
    })
}

fn inspect_provenance() -> Result<Provenance, Box<dyn Error>> {
    let commit = command_output("git", &["rev-parse", "HEAD"])?;
    let diff = Command::new("git")
        .args(["diff", "--binary", "HEAD"])
        .output()?;
    if !diff.status.success() {
        return Err("git diff failed while collecting provenance".into());
    }
    let untracked = command_output("git", &["ls-files", "--others", "--exclude-standard"])?;
    let mut hasher = blake3::Hasher::new();
    hasher.update(&diff.stdout);
    for path in untracked.lines().filter(|path| !path.is_empty()) {
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
        hasher.update(&fs::read(path)?);
        hasher.update(&[0]);
    }
    let dirty = !diff.stdout.is_empty() || !untracked.is_empty();
    Ok(Provenance {
        git_commit: commit,
        dirty,
        dirty_diff_blake3: hasher.finalize().to_hex().to_string(),
        rustc: command_output("/Users/yi/.cargo/bin/rustc", &["--version"])
            .unwrap_or_else(|_| "unavailable".to_owned()),
        os: std::env::consts::OS.to_owned(),
        architecture: std::env::consts::ARCH.to_owned(),
    })
}

fn measure_instrumentation(
    connection: &Connection,
    commit: HashId,
) -> Result<InstrumentationMeasurement, Box<dyn Error>> {
    const ITERATIONS: u64 = 25;
    performance::set_enabled(false);
    let disabled = measure_snapshot_resolution(connection, commit, ITERATIONS)?;
    performance::reset();
    performance::set_enabled(true);
    let enabled = measure_snapshot_resolution(connection, commit, ITERATIONS)?;
    performance::set_enabled(false);
    Ok(InstrumentationMeasurement {
        iterations: ITERATIONS,
        disabled_micros: disabled.as_micros(),
        enabled_micros: enabled.as_micros(),
        overhead_micros: enabled.as_micros() as i128 - disabled.as_micros() as i128,
    })
}

fn measure_snapshot_resolution(
    connection: &Connection,
    commit: HashId,
    iterations: u64,
) -> Result<Duration, Box<dyn Error>> {
    let started = Instant::now();
    for _ in 0..iterations {
        let _ = Snapshot::resolve(connection, commit)?;
    }
    Ok(started.elapsed())
}

fn selected_cases(
    _connection: &Connection,
    fixture: &FixtureManifest,
    selected: &[String],
) -> Result<Vec<CaseSpec>, Box<dyn Error>> {
    let target = i64::try_from(fixture.scale_node_count / 2 + 1)?;
    let range_upper = target.saturating_add(1_000);
    let delta_target = target.saturating_add(i64::try_from(fixture.scale_node_count)?);
    let delta_upper = delta_target.saturating_add(1_000);
    let all = [
        CaseSpec {
            name: "low_degree_one_hop",
            query: "MATCH (:ScaleLowDegree)-[:SCALE_LINK]->(m) RETURN 1",
            parameters: BTreeMap::new(),
            report_parameters: BTreeMap::new(),
            expected_rows: 1,
        },
        CaseSpec {
            name: "high_degree_one_hop",
            query: "MATCH (:ScaleHub)-[:SCALE_LINK]->(m) RETURN 1",
            parameters: BTreeMap::new(),
            report_parameters: BTreeMap::new(),
            expected_rows: fixture.hub_degree,
        },
        CaseSpec {
            name: "label_scan",
            query: "MATCH (:ScaleNode) RETURN 1",
            parameters: BTreeMap::new(),
            report_parameters: BTreeMap::new(),
            expected_rows: fixture.scale_node_count,
        },
        CaseSpec {
            name: "indexed_equality",
            query: "MATCH (n:ScaleNode) WHERE n.scaleId = $target RETURN n.scaleId",
            parameters: BTreeMap::from([("target".to_owned(), Value::Integer(target))]),
            report_parameters: BTreeMap::from([("target".to_owned(), target.to_string())]),
            expected_rows: 1,
        },
        CaseSpec {
            name: "indexed_range",
            query: "MATCH (n:ScaleNode) WHERE n.scaleId >= $lower AND n.scaleId < $upper RETURN n.scaleId",
            parameters: BTreeMap::from([
                ("lower".to_owned(), Value::Integer(target)),
                ("upper".to_owned(), Value::Integer(range_upper)),
            ]),
            report_parameters: BTreeMap::from([
                ("lower".to_owned(), target.to_string()),
                ("upper".to_owned(), range_upper.to_string()),
            ]),
            expected_rows: 1_000,
        },
        CaseSpec {
            name: "indexed_delta_old_equality",
            query: "MATCH (n:ScaleNode) WHERE n.scaleId = $target RETURN n.scaleId",
            parameters: BTreeMap::from([("target".to_owned(), Value::Integer(target))]),
            report_parameters: BTreeMap::from([("target".to_owned(), target.to_string())]),
            expected_rows: 0,
        },
        CaseSpec {
            name: "indexed_delta_range",
            query: "MATCH (n:ScaleNode) WHERE n.scaleId >= $lower AND n.scaleId < $upper RETURN n.scaleId",
            parameters: BTreeMap::from([
                ("lower".to_owned(), Value::Integer(delta_target)),
                ("upper".to_owned(), Value::Integer(delta_upper)),
            ]),
            report_parameters: BTreeMap::from([
                ("lower".to_owned(), delta_target.to_string()),
                ("upper".to_owned(), delta_upper.to_string()),
            ]),
            expected_rows: 1_000,
        },
    ];
    let mut result = Vec::new();
    for name in selected {
        let case = all
            .iter()
            .find(|case| case.name == name)
            .ok_or_else(|| format!("unknown Phase 11 performance case {name}"))?;
        result.push(case.clone());
    }
    Ok(result)
}

fn run_case(
    connection: &Connection,
    config: &Config,
    fixture: &FixtureManifest,
    spec: &CaseSpec,
) -> Result<CaseReport, Box<dyn Error>> {
    let mut samples = Vec::with_capacity(config.samples);
    for _ in 0..config.samples {
        let sample = run_query_sample(connection, config.batch_size, spec)?;
        if config.enforce_gates {
            enforce_sample_gates(spec, &sample)?;
        }
        samples.push(sample);
    }
    let explain = explain_query(connection, spec)?;
    let physical_probe = match spec.name {
        "low_degree_one_hop" => Some(adjacency_probe(connection, fixture, false)?),
        "high_degree_one_hop" => Some(adjacency_probe(connection, fixture, true)?),
        _ => None,
    };
    Ok(CaseReport {
        name: spec.name.to_owned(),
        adapter: "core".to_owned(),
        cache_state: "connection-local-current".to_owned(),
        query: spec.query.to_owned(),
        parameters: spec.report_parameters.clone(),
        sample_count: config.samples,
        expected_rows: spec.expected_rows,
        samples,
        explain,
        physical_probe,
    })
}

fn enforce_sample_gates(spec: &CaseSpec, sample: &QuerySample) -> Result<(), Box<dyn Error>> {
    if sample.counters.resolved_state_builds != 1 {
        return Err(format!(
            "{} resolved Snapshot state {} times; expected exactly 1",
            spec.name, sample.counters.resolved_state_builds
        )
        .into());
    }
    if matches!(
        spec.name,
        "indexed_equality" | "indexed_range" | "indexed_delta_old_equality" | "indexed_delta_range"
    ) && sample.counters.standard_index_builds != 0
    {
        return Err(format!(
            "{} rebuilt {} Standard Index generations; expected 0",
            spec.name, sample.counters.standard_index_builds
        )
        .into());
    }
    Ok(())
}

fn run_query_sample(
    connection: &Connection,
    batch_size: usize,
    spec: &CaseSpec,
) -> Result<QuerySample, Box<dyn Error>> {
    performance::reset();
    performance::set_enabled(true);
    let total_started = Instant::now();
    let prepare_started = Instant::now();
    let prepared = prepare(
        connection,
        spec.query,
        spec.parameters.clone(),
        ExecutionOptions::default(),
    )?;
    let prepare_micros = prepare_started.elapsed().as_micros();
    let consume_started = Instant::now();
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = 0_u64;
    let mut first_row_micros = None;
    let mut checksum = blake3::Hasher::new();
    let summary = loop {
        let batch = cursor.next_batch(connection, batch_size)?;
        if first_row_micros.is_none() && !batch.rows.is_empty() {
            first_row_micros = Some(total_started.elapsed().as_micros());
        }
        for row in batch.rows {
            checksum.update(format!("{row:?}\n").as_bytes());
            rows = rows.saturating_add(1);
        }
        if batch.done {
            break cursor.complete(connection)?;
        }
    };
    performance::set_enabled(false);
    if rows != spec.expected_rows {
        return Err(format!("returned {rows} rows, expected {}", spec.expected_rows).into());
    }
    Ok(QuerySample {
        total_micros: total_started.elapsed().as_micros(),
        prepare_micros,
        consume_micros: consume_started.elapsed().as_micros(),
        first_row_micros,
        rows,
        checksum_blake3: checksum.finalize().to_hex().to_string(),
        db_hits: summary.metrics.db_hits,
        counters: performance::snapshot().into(),
    })
}

fn explain_query(connection: &Connection, spec: &CaseSpec) -> Result<String, Box<dyn Error>> {
    let prepared = prepare(
        connection,
        &format!("EXPLAIN {}", spec.query),
        spec.parameters.clone(),
        ExecutionOptions::default(),
    )?;
    let mut cursor = QueryCursor::new(prepared);
    let mut output = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 256)?;
        output.extend(batch.rows);
        if batch.done {
            let _ = cursor.complete(connection)?;
            break;
        }
    }
    Ok(format!("{output:?}"))
}

fn adjacency_probe(
    connection: &Connection,
    fixture: &FixtureManifest,
    hub: bool,
) -> Result<PhysicalProbe, Box<dyn Error>> {
    let commit = parse_commit(&fixture.checkpoint_commit)?;
    let label = if hub { "ScaleHub" } else { "ScaleLowDegree" };
    let node_id: i64 = connection.query_row(
        "SELECT labels.node_id FROM main._lithograph_cp_labels AS labels \
         JOIN main._lithograph_labels AS dictionary ON dictionary.id = labels.label_id \
         WHERE labels.commit_id = ?1 AND dictionary.name = ?2 LIMIT 1",
        params![commit.as_bytes().as_slice(), label],
        |row| row.get(0),
    )?;
    let type_id: i64 = connection.query_row(
        "SELECT id FROM main._lithograph_rel_types WHERE name = 'SCALE_LINK'",
        [],
        |row| row.get(0),
    )?;
    let sql = "SELECT relationship_id, source_id, type_id, target_id \
               FROM main._lithograph_cp_relationships INDEXED BY _lithograph_cp_rel_out \
               WHERE commit_id = ?1 AND source_id = ?2 AND type_id = ?3 \
               ORDER BY target_id, relationship_id LIMIT 256";
    let query_plan = explain_sql(connection, sql, commit, node_id, type_id)?;
    let mut statement = connection.prepare(sql)?;
    let rows = statement
        .query_map(
            params![commit.as_bytes().as_slice(), node_id, type_id],
            |_| Ok(()),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(PhysicalProbe {
        sql: sql.to_owned(),
        query_plan,
        vm_steps: statement.get_status(StatementStatus::VmStep),
        fullscan_steps: statement.get_status(StatementStatus::FullscanStep),
        sorts: statement.get_status(StatementStatus::Sort),
        rows: rows.len() as u64,
    })
}

fn explain_sql(
    connection: &Connection,
    sql: &str,
    commit: HashId,
    node_id: i64,
    type_id: i64,
) -> Result<Vec<String>, Box<dyn Error>> {
    let mut statement = connection.prepare(&format!("EXPLAIN QUERY PLAN {sql}"))?;
    Ok(statement
        .query_map(
            params![commit.as_bytes().as_slice(), node_id, type_id],
            |row| row.get::<_, String>(3),
        )?
        .collect::<Result<Vec<_>, _>>()?)
}

fn parse_commit(value: &str) -> Result<HashId, Box<dyn Error>> {
    Ok(HashId::from_hex(
        value.strip_prefix("commit/").unwrap_or(value),
    )?)
}

fn command_output(command: &str, arguments: &[&str]) -> Result<String, Box<dyn Error>> {
    let output = Command::new(command).args(arguments).output()?;
    if !output.status.success() {
        return Err(format!("{command} exited with {}", output.status).into());
    }
    Ok(String::from_utf8(output.stdout)?.trim().to_owned())
}

fn env_usize(name: &str, default: usize) -> Result<usize, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
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
