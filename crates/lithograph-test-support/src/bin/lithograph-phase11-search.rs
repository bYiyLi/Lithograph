use std::collections::{BTreeMap, BTreeSet};
use std::error::Error;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

use lithograph_core::cypher::Value;
use lithograph_core::performance;
use lithograph_core::query::{ExecutionOptions, QueryCursor, SnapshotSelector, prepare};
use lithograph_core::storage::{
    STORAGE_FORMAT, SearchScaleFixture, SearchScaleFixtureSpec, branch_head, create_checkpoint,
    create_storage_schema, initialize_connection_state, initialize_root, integrity_check,
    seed_search_scale_fixture,
};
use rusqlite::Connection;
use serde::Serialize;

const DEFAULT_DOCUMENTS: u64 = 1_000_000;
const DEFAULT_HIGH_DIM_DOCUMENTS: u64 = 100_000;
const DEFAULT_LOW_DIMENSION: u64 = 128;
const DEFAULT_HIGH_DIMENSION: u64 = 1_536;

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchReport {
    schema_version: u32,
    provenance: SearchProvenance,
    run_mode: &'static str,
    database: String,
    database_bytes: u64,
    document_count: u64,
    high_dimension_count: u64,
    low_dimension: u64,
    high_dimension: u64,
    vector_coordinate_type: &'static str,
    vector_similarity: &'static str,
    vector_top_k: u64,
    vector_hnsw_m: u64,
    vector_hnsw_ef_construction: u64,
    vector_search_expansion_factor: f64,
    visibility_rule: &'static str,
    visibility_selectivity: f64,
    text_template: &'static str,
    text_min_bytes: usize,
    text_max_bytes: usize,
    fixture_seed: &'static str,
    fixture_commit: String,
    index_commit: String,
    machine: String,
    rustc: String,
    sqlite: String,
    setup: Vec<Measurement>,
    vector: Vec<VectorMeasurement>,
    fulltext: Vec<FullTextMeasurement>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct SearchProvenance {
    git_commit: String,
    dirty: bool,
    dirty_diff_blake3: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct Measurement {
    name: String,
    elapsed_micros: u128,
    detail: String,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct VectorMeasurement {
    index: String,
    cache_state: &'static str,
    dimension: u64,
    corpus_count: u64,
    target_scale_id: u64,
    visible_only: bool,
    historical: bool,
    elapsed_micros: u128,
    oracle_micros: u128,
    vector_cache_builds: u64,
    vector_cache_build_micros: u64,
    vector_cache_entry_loads: u64,
    vector_cache_entry_load_micros: u64,
    vector_cache_search_micros: u64,
    returned_ids: Vec<u64>,
    exact_ids: Vec<u64>,
    recall_at_10: f64,
    max_score_error: f64,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "camelCase")]
struct FullTextMeasurement {
    token: String,
    cache_state: &'static str,
    expected_scale_ids: Vec<u64>,
    visible_only: bool,
    historical: bool,
    elapsed_micros: u128,
    returned_scale_ids: Vec<u64>,
}

#[derive(Clone)]
struct Config {
    root: PathBuf,
    database: PathBuf,
    documents: u64,
    high_dim_documents: u64,
    low_dimension: u64,
    high_dimension: u64,
    progress_interval: u64,
    reuse: bool,
    hnsw_m: u64,
    hnsw_ef_construction: u64,
    search_expansion_factor: f64,
}

fn main() -> Result<(), Box<dyn Error>> {
    let config = Config::from_env()?;
    fs::create_dir_all(&config.root)?;
    let mut setup = Vec::new();
    let (connection, fixture) = open_fixture(&config, &mut setup)?;
    connection.execute_batch("PRAGMA journal_mode=WAL; PRAGMA synchronous=NORMAL")?;
    let index_commit = install_search_indexes(&connection, &config, &mut setup)?;
    let mut vector = run_vector_workloads(&connection, &config)?;
    let fulltext = run_fulltext_workloads(&connection, &config, index_commit)?;
    vector.push(measure_vector_query(
        &connection,
        "phase11_search_vec128",
        "Phase11SearchDocument",
        config.low_dimension,
        config.documents,
        config.documents / 2 + 1,
        false,
        Some(index_commit),
    )?);
    let report = SearchReport {
        schema_version: 1,
        provenance: inspect_search_provenance()?,
        run_mode: if config.reuse {
            "reopen-existing-derived-state"
        } else {
            "fresh-build-and-query"
        },
        database: config.database.display().to_string(),
        database_bytes: sqlite_files_size(&config.database)?,
        document_count: config.documents,
        high_dimension_count: config.high_dim_documents,
        low_dimension: config.low_dimension,
        high_dimension: config.high_dimension,
        vector_coordinate_type: "FLOAT32",
        vector_similarity: "cosine",
        vector_top_k: 10,
        vector_hnsw_m: config.hnsw_m,
        vector_hnsw_ef_construction: config.hnsw_ef_construction,
        vector_search_expansion_factor: config.search_expansion_factor,
        visibility_rule: "Phase11SearchVisible on odd scaleId",
        visibility_selectivity: 0.5,
        text_template: "phase eleven search document {offset} common bucket{offset%1000} needle{offset}",
        text_min_bytes: search_document_text_len(0),
        text_max_bytes: search_document_text_len(config.documents.saturating_sub(1)),
        fixture_seed: "phase11-search-deterministic-v1",
        fixture_commit: format!("commit/{}", fixture.commit.to_hex()),
        index_commit: format!("commit/{}", index_commit.to_hex()),
        machine: command_output("uname", &["-a"]),
        rustc: command_output("/Users/yi/.cargo/bin/rustc", &["--version"]),
        sqlite: connection.query_row("SELECT sqlite_version()", [], |row| row.get(0))?,
        setup,
        vector,
        fulltext,
    };
    let path = config.root.join("report.json");
    fs::write(&path, serde_json::to_vec_pretty(&report)?)?;
    println!("{}", serde_json::to_string_pretty(&report)?);
    eprintln!("phase11 Search report: {}", path.display());
    Ok(())
}

fn search_document_text_len(offset: u64) -> usize {
    format!(
        "phase eleven search document {offset} common bucket{} needle{offset}",
        offset % 1_000
    )
    .len()
}

impl Config {
    fn from_env() -> Result<Self, Box<dyn Error>> {
        let root = PathBuf::from(
            std::env::var("LITHOGRAPH_PHASE11_SEARCH_DIR")
                .unwrap_or_else(|_| "target/phase11-search".to_owned()),
        );
        let documents = env_u64("LITHOGRAPH_PHASE11_SEARCH_DOCUMENTS", DEFAULT_DOCUMENTS)?;
        let high_dim_documents = env_u64(
            "LITHOGRAPH_PHASE11_SEARCH_HIGH_DIM_DOCUMENTS",
            DEFAULT_HIGH_DIM_DOCUMENTS,
        )?;
        let low_dimension = env_u64(
            "LITHOGRAPH_PHASE11_SEARCH_LOW_DIMENSION",
            DEFAULT_LOW_DIMENSION,
        )?;
        let high_dimension = env_u64(
            "LITHOGRAPH_PHASE11_SEARCH_HIGH_DIMENSION",
            DEFAULT_HIGH_DIMENSION,
        )?;
        if high_dim_documents > documents {
            return Err("high-dimension corpus cannot exceed total Search documents".into());
        }
        Ok(Self {
            database: root.join("search.sqlite"),
            root,
            documents,
            high_dim_documents,
            low_dimension,
            high_dimension,
            progress_interval: env_u64("LITHOGRAPH_PHASE11_SEARCH_PROGRESS", 10_000)?,
            reuse: env_bool("LITHOGRAPH_PHASE11_SEARCH_REUSE")?,
            hnsw_m: env_u64("LITHOGRAPH_PHASE11_SEARCH_HNSW_M", 8)?,
            hnsw_ef_construction: env_u64("LITHOGRAPH_PHASE11_SEARCH_HNSW_EF", 64)?,
            search_expansion_factor: env_f64("LITHOGRAPH_PHASE11_SEARCH_EXPANSION_FACTOR", 4.0)?,
        })
    }
}

fn open_fixture(
    config: &Config,
    setup: &mut Vec<Measurement>,
) -> Result<(Connection, SearchScaleFixture), Box<dyn Error>> {
    if config.reuse {
        let connection = Connection::open(&config.database)?;
        let fixture = existing_fixture(&connection, config)?;
        setup.push(Measurement {
            name: "reuse_fixture".to_owned(),
            elapsed_micros: 0,
            detail: format!("commit/{}", fixture.commit.to_hex()),
        });
        return Ok((connection, fixture));
    }
    remove_sqlite_files(&config.database)?;
    let connection = Connection::open(&config.database)?;
    initialize_database(&connection)?;
    let started = Instant::now();
    let fixture = seed_search_scale_fixture(
        &connection,
        SearchScaleFixtureSpec {
            document_count: config.documents,
            high_dimension_count: config.high_dim_documents,
            low_dimension: config.low_dimension,
            high_dimension: config.high_dimension,
            progress_interval: config.progress_interval,
        },
        |phase, current, total| eprintln!("phase11-search {phase}: {current}/{total}"),
    )?;
    setup.push(measurement(
        "canonical_fixture_seed",
        started,
        format!("commit/{}", fixture.commit.to_hex()),
    ));
    let started = Instant::now();
    create_checkpoint(&connection, fixture.commit)?;
    setup.push(measurement(
        "checkpoint_rebuild",
        started,
        "canonical Search corpus checkpoint".to_owned(),
    ));
    let started = Instant::now();
    let issues = integrity_check(&connection)?;
    if !issues.is_empty() {
        return Err(format!("Search fixture integrity failed: {issues:?}").into());
    }
    setup.push(measurement(
        "integrity_check",
        started,
        "0 issues".to_owned(),
    ));
    Ok((connection, fixture))
}

fn initialize_database(connection: &Connection) -> Result<(), Box<dyn Error>> {
    connection.execute_batch(
        "PRAGMA journal_mode=OFF; PRAGMA synchronous=OFF; PRAGMA temp_store=FILE; PRAGMA cache_size=-65536;\
         CREATE TABLE main._lithograph_meta(\
             id INTEGER PRIMARY KEY CHECK(id=1), magic TEXT NOT NULL, \
             database_id TEXT NOT NULL, storage_format INTEGER NOT NULL);",
    )?;
    connection.execute(
        "INSERT INTO main._lithograph_meta(id, magic, database_id, storage_format) \
         VALUES(1, 'lithograph-format-v1', '00000000-0000-4000-8000-000000001111', ?1)",
        [STORAGE_FORMAT],
    )?;
    create_storage_schema(connection)?;
    initialize_root(connection)?;
    initialize_connection_state(connection)?;
    Ok(())
}

fn existing_fixture(
    connection: &Connection,
    config: &Config,
) -> Result<SearchScaleFixture, Box<dyn Error>> {
    let document_label: i64 = connection.query_row(
        "SELECT id FROM main._lithograph_labels WHERE name='Phase11SearchDocument'",
        [],
        |row| row.get(0),
    )?;
    let checkpoint_bytes: Vec<u8> = connection.query_row(
        "SELECT commit_id FROM main._lithograph_cp_labels WHERE label_id = ?1 \
         GROUP BY commit_id ORDER BY count(*) DESC LIMIT 1",
        [document_label],
        |row| row.get(0),
    )?;
    let commit = lithograph_core::storage::HashId::from_slice(&checkpoint_bytes)?;
    let high_dimension_label: i64 = connection.query_row(
        "SELECT id FROM main._lithograph_labels WHERE name='Phase11SearchHighDimension'",
        [],
        |row| row.get(0),
    )?;
    let visible_label: i64 = connection.query_row(
        "SELECT id FROM main._lithograph_labels WHERE name='Phase11SearchVisible'",
        [],
        |row| row.get(0),
    )?;
    let count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_cp_labels WHERE commit_id = ?1 AND label_id = ?2",
        rusqlite::params![checkpoint_bytes.as_slice(), document_label],
        |row| row.get(0),
    )?;
    if u64::try_from(count)? != config.documents {
        return Err(format!(
            "reused Search fixture has {count} documents, expected {}",
            config.documents
        )
        .into());
    }
    let high_dimension_count: i64 = connection.query_row(
        "SELECT count(*) FROM main._lithograph_cp_labels WHERE commit_id = ?1 AND label_id = ?2",
        rusqlite::params![checkpoint_bytes.as_slice(), high_dimension_label],
        |row| row.get(0),
    )?;
    if u64::try_from(high_dimension_count)? != config.high_dim_documents {
        return Err(format!(
            "reused Search fixture has {high_dimension_count} high-dimension documents, expected {}",
            config.high_dim_documents
        )
        .into());
    }
    let first_node: i64 = connection.query_row(
        "SELECT min(node_id) FROM main._lithograph_cp_nodes WHERE commit_id = ?1",
        [checkpoint_bytes.as_slice()],
        |row| row.get(0),
    )?;
    Ok(SearchScaleFixture {
        root: lithograph_core::storage::root_commit(connection)?,
        commit,
        first_node,
        document_label,
        visible_label,
    })
}

fn install_search_indexes(
    connection: &Connection,
    config: &Config,
    setup: &mut Vec<Measurement>,
) -> Result<lithograph_core::storage::HashId, Box<dyn Error>> {
    let fulltext = "CREATE FULLTEXT INDEX phase11_search_text IF NOT EXISTS FOR (n:Phase11SearchDocument) ON EACH [n.text]";
    measure_ddl(connection, setup, "fulltext_definition", fulltext)?;
    let low = vector_ddl(
        "phase11_search_vec128",
        "Phase11SearchDocument",
        "embedding128",
        config.low_dimension,
        config,
    );
    measure_ddl(connection, setup, "vector128_definition", &low)?;
    let high = vector_ddl(
        "phase11_search_vec1536",
        "Phase11SearchHighDimension",
        "embedding1536",
        config.high_dimension,
        config,
    );
    measure_ddl(connection, setup, "vector1536_definition", &high)?;
    Ok(branch_head(connection, "main")?)
}

fn vector_ddl(name: &str, label: &str, property: &str, dimension: u64, config: &Config) -> String {
    format!(
        "CREATE VECTOR INDEX {name} IF NOT EXISTS FOR (n:{label}) ON (n.{property}) \
         OPTIONS {{indexConfig:{{`vector.dimensions`:{dimension}, `vector.similarity_function`:'cosine', \
         `vector.default_search_expansion_factor`:{}, `vector.hnsw.m`:{}, `vector.hnsw.ef_construction`:{}}}}}",
        config.search_expansion_factor, config.hnsw_m, config.hnsw_ef_construction
    )
}

fn measure_ddl(
    connection: &Connection,
    setup: &mut Vec<Measurement>,
    name: &str,
    query: &str,
) -> Result<(), Box<dyn Error>> {
    let started = Instant::now();
    let _ = execute(connection, query, ExecutionOptions::default())?;
    setup.push(measurement(
        name,
        started,
        "Schema definition Commit".to_owned(),
    ));
    Ok(())
}

fn run_vector_workloads(
    connection: &Connection,
    config: &Config,
) -> Result<Vec<VectorMeasurement>, Box<dyn Error>> {
    let mut measurements = Vec::new();
    let low_targets = sample_targets(config.documents);
    for target in low_targets {
        measurements.push(measure_vector_query(
            connection,
            "phase11_search_vec128",
            "Phase11SearchDocument",
            config.low_dimension,
            config.documents,
            target,
            false,
            None,
        )?);
    }
    let visible_target = nearest_visible_target(config.documents / 2 + 1, config.documents);
    measurements.push(measure_vector_query(
        connection,
        "phase11_search_vec128",
        "Phase11SearchDocument",
        config.low_dimension,
        config.documents,
        visible_target,
        true,
        None,
    )?);
    for target in sample_targets(config.high_dim_documents) {
        measurements.push(measure_vector_query(
            connection,
            "phase11_search_vec1536",
            "Phase11SearchHighDimension",
            config.high_dimension,
            config.high_dim_documents,
            target,
            false,
            None,
        )?);
    }
    Ok(measurements)
}

#[allow(
    clippy::too_many_arguments,
    reason = "Search acceptance records index, dimension, corpus, target, and visibility explicitly"
)]
fn measure_vector_query(
    connection: &Connection,
    index: &str,
    label: &str,
    dimension: u64,
    corpus_count: u64,
    target_scale_id: u64,
    visible_only: bool,
    snapshot: Option<lithograph_core::storage::HashId>,
) -> Result<VectorMeasurement, Box<dyn Error>> {
    let vector = vector_literal(target_scale_id, corpus_count, dimension);
    let query = format!(
        "MATCH (n:{label}) SEARCH n IN (VECTOR INDEX {index} FOR {vector} LIMIT 10) \
         SCORE AS score RETURN n.scaleId, score"
    );
    let mut options = if visible_only {
        ExecutionOptions::parse_text(
            r#"{"graphView":{"requireAllLabels":["Phase11SearchVisible"]}}"#,
        )?
    } else {
        ExecutionOptions::default()
    };
    if let Some(commit) = snapshot {
        options.snapshot = SnapshotSelector::Commit(commit.to_hex());
    }
    performance::reset();
    performance::set_enabled(true);
    let started = Instant::now();
    let rows = execute(connection, &query, options);
    let elapsed_micros = started.elapsed().as_micros();
    let counters = performance::snapshot();
    performance::set_enabled(false);
    let rows = rows?;
    let parsed = parse_vector_rows(&rows)?;
    let oracle_started = Instant::now();
    let exact = exact_vector_top10(corpus_count, target_scale_id, visible_only);
    let oracle_micros = oracle_started.elapsed().as_micros();
    let exact_ids = exact.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let returned_ids = parsed.iter().map(|(id, _)| *id).collect::<Vec<_>>();
    let recall_at_10 =
        tie_aware_recall_at_10(&parsed, &exact, corpus_count, target_scale_id, visible_only);
    if recall_at_10 < 0.95 {
        return Err(format!(
            "{index} target {target_scale_id} recall@10={recall_at_10:.3}; returned={returned_ids:?}, exact={exact_ids:?}"
        )
        .into());
    }
    let max_score_error = parsed
        .iter()
        .map(|(id, score)| (score - cosine_score(*id, target_scale_id, corpus_count)).abs())
        .fold(0.0_f64, f64::max);
    if !max_score_error.is_finite() {
        return Err(format!("{index} returned a non-finite score error").into());
    }
    if parsed.windows(2).any(|window| {
        window[0].1 < window[1].1 || (window[0].1 == window[1].1 && window[0].0 > window[1].0)
    }) {
        return Err(format!("{index} rows are not in score/id order: {parsed:?}").into());
    }
    if returned_ids.iter().copied().collect::<BTreeSet<_>>().len() != returned_ids.len() {
        return Err(format!("{index} returned duplicate owners: {returned_ids:?}").into());
    }
    Ok(VectorMeasurement {
        index: index.to_owned(),
        cache_state: if counters.vector_cache_builds == 0 {
            "same-connection-warm"
        } else {
            "new-connection-cold-build"
        },
        dimension,
        corpus_count,
        target_scale_id,
        visible_only,
        historical: snapshot.is_some(),
        elapsed_micros,
        oracle_micros,
        vector_cache_builds: counters.vector_cache_builds,
        vector_cache_build_micros: counters.vector_cache_build_micros,
        vector_cache_entry_loads: counters.vector_cache_entry_loads,
        vector_cache_entry_load_micros: counters.vector_cache_entry_load_micros,
        vector_cache_search_micros: counters.vector_cache_search_micros,
        returned_ids,
        exact_ids,
        recall_at_10,
        max_score_error,
    })
}

fn parse_vector_rows(rows: &[Vec<Value>]) -> Result<Vec<(u64, f64)>, Box<dyn Error>> {
    rows.iter()
        .map(|row| {
            let id = match row.first() {
                Some(Value::Integer(value)) => u64::try_from(*value)?,
                value => return Err(format!("vector scaleId is not Integer: {value:?}").into()),
            };
            let score = match row.get(1) {
                Some(Value::Float(value)) => *value,
                value => return Err(format!("vector score is not Float: {value:?}").into()),
            };
            Ok((id, score))
        })
        .collect()
}

fn sample_targets(count: u64) -> [u64; 3] {
    [1, count / 2 + 1, count]
}

fn nearest_visible_target(target: u64, count: u64) -> u64 {
    let target = target.clamp(1, count);
    if target % 2 == 1 {
        target
    } else if target < count {
        target + 1
    } else {
        target - 1
    }
}

fn vector_literal(target_scale_id: u64, count: u64, dimension: u64) -> String {
    let second = vector_second(target_scale_id, count);
    let mut values = Vec::with_capacity(usize::try_from(dimension).unwrap_or(2));
    values.push("1.0".to_owned());
    values.push(format!("{second:.9}"));
    values.extend((2..dimension).map(|_| "0.0".to_owned()));
    format!("vector([{}],{dimension},FLOAT32)", values.join(","))
}

fn exact_vector_top10(count: u64, target: u64, visible_only: bool) -> Vec<(u64, f64)> {
    let mut values = (1..=count)
        .filter(|id| !visible_only || id % 2 == 1)
        .map(|id| (id, cosine_score(id, target, count)))
        .collect::<Vec<_>>();
    values.sort_by(|left, right| {
        right
            .1
            .total_cmp(&left.1)
            .then_with(|| left.0.cmp(&right.0))
    });
    values.truncate(10);
    values
}

fn tie_aware_recall_at_10(
    returned: &[(u64, f64)],
    exact: &[(u64, f64)],
    corpus_count: u64,
    target_scale_id: u64,
    visible_only: bool,
) -> f64 {
    let Some((_, cutoff)) = exact.last() else {
        return f64::from(returned.is_empty());
    };
    let strict = exact
        .iter()
        .filter(|(_, score)| score > cutoff)
        .map(|(id, _)| *id)
        .collect::<BTreeSet<_>>();
    let strict_matches = returned
        .iter()
        .filter(|(id, _)| strict.contains(id))
        .count();
    let tie_slots = exact.len().saturating_sub(strict.len());
    let tie_matches = returned
        .iter()
        .filter(|(id, _)| {
            !strict.contains(id)
                && (!visible_only || *id % 2 == 1)
                && cosine_score(*id, target_scale_id, corpus_count) == *cutoff
        })
        .count()
        .min(tie_slots);
    (strict_matches + tie_matches) as f64 / exact.len() as f64
}

fn cosine_score(candidate: u64, target: u64, count: u64) -> f64 {
    let candidate = vector_second_f32(candidate, count);
    let target = vector_second_f32(target, count);
    let dot = 1.0_f32 + candidate * target;
    let candidate_norm = (1.0_f32 + candidate * candidate).sqrt();
    let target_norm = (1.0_f32 + target * target).sqrt();
    f64::from(((1.0_f32 + dot / (candidate_norm * target_norm)) / 2.0).clamp(0.0, 1.0))
}

fn vector_second(scale_id: u64, count: u64) -> f64 {
    f64::from(vector_second_f32(scale_id, count))
}

fn vector_second_f32(scale_id: u64, count: u64) -> f32 {
    let denominator = count.saturating_sub(1).max(1) as f32;
    scale_id.saturating_sub(1) as f32 / denominator
}

fn run_fulltext_workloads(
    connection: &Connection,
    config: &Config,
    index_commit: lithograph_core::storage::HashId,
) -> Result<Vec<FullTextMeasurement>, Box<dyn Error>> {
    let mut measurements = Vec::new();
    measurements.push(measure_fulltext(
        connection,
        "needle0",
        vec![1],
        false,
        false,
        "new-connection-cold-build",
        ExecutionOptions::default(),
    )?);
    measurements.push(measure_fulltext(
        connection,
        "needle1",
        Vec::new(),
        true,
        false,
        "same-connection-warm",
        ExecutionOptions::parse_text(
            r#"{"graphView":{"requireAllLabels":["Phase11SearchVisible"]}}"#,
        )?,
    )?);
    let midpoint = config.documents / 2;
    let token = format!("needle{midpoint}");
    measurements.push(measure_fulltext(
        connection,
        &token,
        vec![midpoint + 1],
        false,
        false,
        "same-connection-warm",
        ExecutionOptions::default(),
    )?);

    let _ = execute(
        connection,
        "CREATE (:Phase11SearchUnrelated {value:1}) FINISH",
        ExecutionOptions::default(),
    )?;
    let mut historical = ExecutionOptions::default();
    historical.snapshot = SnapshotSelector::Commit(index_commit.to_hex());
    measurements.push(measure_fulltext(
        connection,
        "needle0",
        vec![1],
        false,
        true,
        "same-connection-warm",
        historical,
    )?);
    Ok(measurements)
}

fn measure_fulltext(
    connection: &Connection,
    token: &str,
    expected: Vec<u64>,
    visible_only: bool,
    historical: bool,
    cache_state: &'static str,
    options: ExecutionOptions,
) -> Result<FullTextMeasurement, Box<dyn Error>> {
    let query = format!(
        "CALL db.index.fulltext.queryNodes('phase11_search_text', '{token}') \
         YIELD node RETURN node.scaleId ORDER BY node.scaleId"
    );
    let started = Instant::now();
    let rows = execute(connection, &query, options)?;
    let elapsed_micros = started.elapsed().as_micros();
    let returned = rows
        .iter()
        .map(|row| match row.first() {
            Some(Value::Integer(value)) => u64::try_from(*value).map_err(Into::into),
            value => Err(format!("full-text scaleId is not Integer: {value:?}").into()),
        })
        .collect::<Result<Vec<_>, Box<dyn Error>>>()?;
    if returned != expected {
        return Err(format!(
            "full-text token {token} returned {returned:?}, expected {expected:?}"
        )
        .into());
    }
    Ok(FullTextMeasurement {
        token: token.to_owned(),
        cache_state,
        expected_scale_ids: expected,
        visible_only,
        historical,
        elapsed_micros,
        returned_scale_ids: returned,
    })
}

fn execute(
    connection: &Connection,
    query: &str,
    options: ExecutionOptions,
) -> Result<Vec<Vec<Value>>, lithograph_core::query::QueryError> {
    let prepared = prepare(connection, query, BTreeMap::new(), options)?;
    let mut cursor = QueryCursor::new(prepared);
    let mut rows = Vec::new();
    loop {
        let batch = cursor.next_batch(connection, 256)?;
        rows.extend(batch.rows);
        if batch.done {
            cursor.complete(connection)?;
            return Ok(rows);
        }
    }
}

fn measurement(name: &str, started: Instant, detail: String) -> Measurement {
    Measurement {
        name: name.to_owned(),
        elapsed_micros: started.elapsed().as_micros(),
        detail,
    }
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

fn inspect_search_provenance() -> Result<SearchProvenance, Box<dyn Error>> {
    let commit = command_output("git", &["rev-parse", "HEAD"]);
    if commit == "unavailable" {
        return Err("git rev-parse failed while collecting Search provenance".into());
    }
    let diff = Command::new("git")
        .args(["diff", "--binary", "HEAD"])
        .output()?;
    if !diff.status.success() {
        return Err("git diff failed while collecting Search provenance".into());
    }
    let untracked = command_output("git", &["ls-files", "--others", "--exclude-standard"]);
    if untracked == "unavailable" {
        return Err("git ls-files failed while collecting Search provenance".into());
    }
    let mut hasher = blake3::Hasher::new();
    hasher.update(&diff.stdout);
    for path in untracked.lines().filter(|path| !path.is_empty()) {
        hasher.update(path.as_bytes());
        hasher.update(&[0]);
        hasher.update(&fs::read(path)?);
        hasher.update(&[0]);
    }
    Ok(SearchProvenance {
        git_commit: commit,
        dirty: !diff.stdout.is_empty() || !untracked.is_empty(),
        dirty_diff_blake3: hasher.finalize().to_hex().to_string(),
    })
}

fn env_u64(name: &str, default: u64) -> Result<u64, Box<dyn Error>> {
    match std::env::var(name) {
        Ok(value) => Ok(value.parse()?),
        Err(std::env::VarError::NotPresent) => Ok(default),
        Err(error) => Err(error.into()),
    }
}

fn env_f64(name: &str, default: f64) -> Result<f64, Box<dyn Error>> {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recall_accepts_cutoff_score_ties_but_rejects_worse_candidates() {
        let count = 1_000_000;
        let target = 500_001;
        let exact = exact_vector_top10(count, target, false);
        let cutoff = exact.last().expect("exact top10").1;
        let tied_ids = [
            499_641, 499_649, 499_675, 499_682, 499_701, 499_718, 499_720, 499_722, 499_726,
            499_745,
        ];
        let tied = tied_ids
            .into_iter()
            .map(|id| (id, cosine_score(id, target, count)))
            .collect::<Vec<_>>();
        assert!(tied.iter().all(|(_, score)| *score == cutoff));
        assert_eq!(
            tie_aware_recall_at_10(&tied, &exact, count, target, false),
            1.0
        );

        let mut worse = tied;
        worse[0] = (1, cosine_score(1, target, count));
        assert!(tie_aware_recall_at_10(&worse, &exact, count, target, false) < 0.95);
    }
}
