use std::collections::BTreeMap;
use std::time::Instant;

use lithograph_core::performance;
use lithograph_core::query::{ExecutionOptions, QueryCursor, SnapshotSelector, prepare};
use lithograph_core::storage::branch_head;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value as JsonValue, json};

use crate::Result;
use crate::config::{Config, Mode};
use crate::fixture::Fixture;
use crate::oracle::Oracle;
use crate::profile;

pub(super) fn run(fixture: &Fixture, config: &Config) -> JsonValue {
    let mut samples = Vec::new();
    let outcome = matrix(fixture, config, &mut samples);
    performance::set_enabled(false);
    let error = outcome.err().map(|error| error.to_string());
    json!({"status": if error.is_none() { "passed" } else { "failed" }, "error": error,
        "samples": samples, "summary": summaries(&samples, error.is_none()),
        "long_mode_process_scope": "long uses 3 fresh connections in this process; RSS acceptance uses three independently invoked single processes",
        "primary_counters_enabled": false, "counter_and_profile_probes_separate": true})
}

fn matrix(fixture: &Fixture, config: &Config, samples: &mut Vec<JsonValue>) -> Result<()> {
    if config.mode == Mode::Full {
        for connection_id in 0..5 {
            let connection = open(fixture)?;
            record(
                &connection,
                fixture,
                config,
                samples,
                "warm_first",
                connection_id,
                0,
            )?;
            for iteration in 0..20 {
                record(
                    &connection,
                    fixture,
                    config,
                    samples,
                    "warm",
                    connection_id,
                    iteration,
                )?;
            }
            close(connection)?;
        }
    }
    let (role, count) = match config.mode {
        Mode::Full => ("fresh", 100),
        Mode::Long => ("long_primary", 3),
        Mode::Single => ("primary", 1),
        Mode::Smoke => ("smoke_primary", 1),
        Mode::Probe => ("probe", 1),
    };
    for iteration in 0..count {
        let connection = open(fixture)?;
        record(&connection, fixture, config, samples, role, iteration, 0)?;
        close(connection)?;
    }
    if matches!(config.mode, Mode::Full | Mode::Smoke | Mode::Long) {
        let connection = open(fixture)?;
        record(&connection, fixture, config, samples, "probe", 0, 0)?;
        close(connection)?;
    }
    Ok(())
}

fn open(fixture: &Fixture) -> Result<Connection> {
    let connection =
        Connection::open_with_flags(&fixture.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if !connection.is_readonly("main")? {
        return Err("actual main open is not readonly".into());
    }
    connection.execute_batch("BEGIN DEFERRED")?;
    if branch_head(&connection, "main")?.to_hex() != fixture.commit.trim_start_matches("commit/") {
        return Err("actual fixture main head differs from its fixed Commit".into());
    }
    Ok(connection)
}

fn close(connection: Connection) -> Result<()> {
    connection.execute_batch("ROLLBACK")?;
    if !connection.is_autocommit() {
        return Err("caller-owned read transaction survived close".into());
    }
    connection.close().map_err(|(_, error)| error)?;
    Ok(())
}

fn record(
    connection: &Connection,
    fixture: &Fixture,
    config: &Config,
    records: &mut Vec<JsonValue>,
    role: &str,
    connection_id: usize,
    iteration: usize,
) -> Result<()> {
    let instrumented = role == "probe";
    performance::reset();
    performance::set_enabled(instrumented);
    let outcome = sample(connection, fixture, config, instrumented);
    performance::set_enabled(false);
    match outcome {
        Ok(sample) => {
            records.push(json!({"role": role, "connection": connection_id, "iteration": iteration, "sample": sample}));
            Ok(())
        }
        Err(error) => {
            records.push(
                json!({"role": role, "connection": connection_id, "iteration": iteration,
                "sample": {"status": "failed", "error": error.to_string()}}),
            );
            Err(error)
        }
    }
}

fn sample(
    connection: &Connection,
    fixture: &Fixture,
    config: &Config,
    instrumented: bool,
) -> Result<JsonValue> {
    let mut options = ExecutionOptions::default();
    options.snapshot =
        SnapshotSelector::Commit(fixture.commit.trim_start_matches("commit/").to_owned());
    options.memory_limit_bytes = config.memory_limit;
    let query = if instrumented {
        format!("PROFILE {}", fixture.query)
    } else {
        fixture.query.clone()
    };
    let environment = environment(connection, config.sqlite_source_id.as_deref())?;
    let mut oracle = Oracle::new(fixture);
    let cpu = cpu_nanos();
    let started = Instant::now();
    let prepared = prepare(connection, &query, BTreeMap::new(), options)?;
    let prepare_us = started.elapsed().as_micros();
    let prepare_cpu_us = elapsed_cpu(cpu);
    if prepared.columns != fixture.columns {
        return Err("actual result columns differ from fixture".into());
    }
    let plan = instrumented.then(|| prepared.physical.explain());
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let (first_row_us, first_row_cpu_us) =
        consume_rows(connection, &mut cursor, &mut oracle, started, cpu)?;
    let summary = cursor.complete(connection)?;
    if summary.metrics.rows != u64::try_from(fixture.expected)? {
        return Err("complete summary row count differs from the actual full oracle".into());
    }
    let db_hits = summary.metrics.db_hits;
    let profile = instrumented
        .then(|| profile::report(&summary, config.case == "full" && !config.before))
        .transpose()?;
    drop(summary);
    drop(cursor);
    let full_us = started.elapsed().as_micros();
    let full_cpu_us = elapsed_cpu(cpu);
    if resources.used() != 0 {
        return Err(format!(
            "{} engine bytes remain after complete/drop",
            resources.used()
        )
        .into());
    }
    let oracle = oracle.finish()?;
    if branch_head(connection, "main")?.to_hex() != fixture.commit.trim_start_matches("commit/") {
        return Err("main head changed while draining fixed Commit".into());
    }
    Ok(
        json!({"status": "passed", "counter_probe": instrumented, "timed_counters_enabled": instrumented,
        "prepare_us": prepare_us, "prepare_cpu_us": prepare_cpu_us, "first_row_us": first_row_us,
        "first_row_cpu_us": first_row_cpu_us, "full_us": full_us, "full_cpu_us": full_cpu_us,
        "consume_us": full_us.saturating_sub(prepare_us), "db_hits": db_hits,
        "resource_peak_bytes": resources.peak(), "resource_used_after_close": resources.used(),
        "oracle": oracle, "physical_plan": plan, "profile": profile,
        "counters": instrumented.then(profile::counters).transpose()?, "sqlite": environment,
        "actual_readonly_open": true, "batch_size_first": 1, "batch_size_following": 256}),
    )
}

fn consume_rows(
    connection: &Connection,
    cursor: &mut QueryCursor,
    oracle: &mut Oracle<'_>,
    started: Instant,
    cpu: Option<u128>,
) -> Result<(Option<u128>, Option<u128>)> {
    let first = cursor.next_batch(connection, 1)?;
    let first_row_us = (!first.rows.is_empty()).then(|| started.elapsed().as_micros());
    let first_row_cpu_us = (!first.rows.is_empty()).then(|| elapsed_cpu(cpu)).flatten();
    let mut done = first.done;
    for row in &first.rows {
        oracle.row(row)?;
    }
    drop(first);
    while !done {
        let batch = cursor.next_batch(connection, 256)?;
        done = batch.done;
        for row in batch.rows {
            oracle.row(&row)?;
        }
    }
    Ok((first_row_us, first_row_cpu_us))
}

fn environment(connection: &Connection, expected_source_id: Option<&str>) -> Result<JsonValue> {
    let (version, source): (String, String) =
        connection.query_row("SELECT sqlite_version(),sqlite_source_id()", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    if let Some(expected) = expected_source_id
        && source != expected
    {
        return Err(format!("SQLite source ID mismatch before graph prepare: expected {expected:?}, actual {source:?}").into());
    }
    let journal: String = connection.query_row("PRAGMA journal_mode", [], |row| row.get(0))?;
    let mut statement = connection.prepare("PRAGMA compile_options")?;
    let options = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let mut settings = BTreeMap::new();
    for name in ["synchronous", "cache_size", "temp_store", "mmap_size"] {
        settings.insert(
            name,
            connection.query_row(&format!("PRAGMA {name}"), [], |row| row.get::<_, i64>(0))?,
        );
    }
    Ok(
        json!({"version": version, "source_id": source, "compile_options": options, "journal_mode": journal, "settings": settings,
            "sqlite_source_id_expected": expected_source_id, "sqlite_source_id_guard": expected_source_id.is_some()}),
    )
}

fn summaries(records: &[JsonValue], case_passed: bool) -> JsonValue {
    let mut result = BTreeMap::new();
    for role in [
        "warm_first",
        "warm",
        "fresh",
        "long_primary",
        "primary",
        "smoke_primary",
        "probe",
    ] {
        let selected = records
            .iter()
            .filter(|record| record["role"] == role)
            .collect::<Vec<_>>();
        let mut fields = BTreeMap::new();
        for field in [
            "prepare_us",
            "first_row_us",
            "full_us",
            "prepare_cpu_us",
            "first_row_cpu_us",
            "full_cpu_us",
        ] {
            let mut raw = selected
                .iter()
                .filter_map(|record| record["sample"][field].as_u64())
                .collect::<Vec<_>>();
            let chronological = raw.clone();
            raw.sort_unstable();
            let median = raw
                .get((raw.len() * 50).div_ceil(100).saturating_sub(1))
                .copied();
            let p95 = (case_passed
                && raw.len() >= 100
                && selected
                    .iter()
                    .all(|record| record["sample"]["status"] == "passed")
                && role != "probe")
                .then(|| raw.get((raw.len() * 95).div_ceil(100) - 1).copied())
                .flatten();
            fields.insert(field, json!({"raw_chronological": chronological, "median": median, "max": raw.last(), "p95": p95}));
        }
        result.insert(
            role,
            json!({"attempts": selected.len(), "microseconds": fields}),
        );
    }
    json!(result)
}

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn cpu_nanos() -> Option<u128> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    u128::try_from(time.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u128::try_from(time.tv_nsec).ok()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn cpu_nanos() -> Option<u128> {
    None
}

fn elapsed_cpu(started: Option<u128>) -> Option<u128> {
    cpu_nanos()?
        .checked_sub(started?)
        .map(|nanos| nanos / 1_000)
}
