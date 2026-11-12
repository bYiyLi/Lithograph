use std::collections::BTreeMap;
use std::path::Path;
use std::time::Instant;

use lithograph_core::cypher::{Value, encode_json};
use lithograph_core::performance;
use lithograph_core::query::{ExecutionOptions, QueryCursor, SnapshotSelector, prepare};
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value as JsonValue, json};

use crate::Result;
use crate::config::Config;
use crate::fixture::{Case, Fixture};

#[cfg(any(target_os = "macos", target_os = "linux"))]
fn process_cpu_nanos() -> Option<u128> {
    let now = rustix::time::clock_gettime(rustix::time::ClockId::ProcessCPUTime);
    u128::try_from(now.tv_sec)
        .ok()?
        .checked_mul(1_000_000_000)?
        .checked_add(u128::try_from(now.tv_nsec).ok()?)
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn process_cpu_nanos() -> Option<u128> {
    None
}

fn elapsed_cpu_micros(started: Option<u128>) -> Option<u128> {
    process_cpu_nanos()?
        .checked_sub(started?)
        .map(|nanos| nanos / 1_000)
}

fn sum_cpu_micros(first: Option<u128>, second: Option<u128>) -> Option<u128> {
    first?.checked_add(second?)
}

pub(super) fn cpu_clock_metadata() -> JsonValue {
    #[cfg(any(target_os = "macos", target_os = "linux"))]
    {
        let resolution = rustix::time::clock_getres(rustix::time::ClockId::ProcessCPUTime);
        json!({"available": true, "source": "rustix::time::clock_gettime(ClockId::ProcessCPUTime)",
               "scope": "CPU of this process, summed across its threads; excludes other processes",
               "resolution": {"seconds": resolution.tv_sec, "nanoseconds": resolution.tv_nsec},
               "boundary": "CPU reads bracket the same prepare/consume/first-pull work as wall time; clock and timing-read overhead retained without subtraction",
               "rounding": "nanosecond deltas truncated to microseconds; 0 does not mean zero cost"})
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        json!({"available": false, "source": null, "reason": "process CPU clock only collected on macOS/Linux; CPU samples are null"})
    }
}

pub(super) fn open_read_only(path: &Path) -> Result<(Connection, u128)> {
    let started = Instant::now();
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    Ok((connection, started.elapsed().as_micros()))
}

fn open_context(path: &Path) -> Result<(Connection, JsonValue)> {
    let (connection, open_us) = open_read_only(path)?;
    let started = Instant::now();
    connection.execute_batch("BEGIN DEFERRED")?;
    Ok((
        connection,
        json!({"open_us": open_us, "read_transaction_begin_us": started.elapsed().as_micros()}),
    ))
}

pub(super) fn run_case(
    fixture: &Fixture,
    case: &Case,
    config: &Config,
    failures: &mut Vec<String>,
) -> JsonValue {
    let failures_before = failures.len();
    performance::set_enabled(false);
    let (connections, iterations, fresh) = config.mode.samples();
    let mut warm = Vec::new();
    let mut reopened = Vec::new();
    for id in 0..connections {
        warm.push(warm_connection(fixture, case, id, iterations, failures));
    }
    for id in 0..fresh {
        reopened.push(fresh_connection(fixture, case, id, false, failures));
    }
    let counter_probe = fresh_connection(fixture, case, 0, true, failures);
    let warm_samples = warm
        .iter()
        .flat_map(|group| group["samples"].as_array().into_iter().flatten())
        .collect::<Vec<_>>();
    let fresh_samples = reopened
        .iter()
        .map(|group| &group["first_query"])
        .collect::<Vec<_>>();
    let case_failures = failures.len() - failures_before;
    json!({"name": case.name, "query": case.query, "columns": case.columns,
              "complete_oracle": case.oracle, "warm": warm, "fresh": reopened,
              "failure_count": case_failures,
              "latency_summary": {"warm": summarize(&warm_samples, case_failures), "fresh": summarize(&fresh_samples, case_failures)},
              "instrumented_fresh_probe": counter_probe})
}

fn warm_connection(
    fixture: &Fixture,
    case: &Case,
    id: usize,
    iterations: usize,
    failures: &mut Vec<String>,
) -> JsonValue {
    let (connection, opening) = match open_context(&fixture.database) {
        Ok(context) => context,
        Err(error) => return structural_error(case, "warm open", error, failures),
    };
    let first = sample_or_error(&connection, fixture, case, false, failures);
    let samples = (0..iterations)
        .map(|_| sample_or_error(&connection, fixture, case, false, failures))
        .collect::<Vec<_>>();
    let cleanup = close_context(&connection, case, failures);
    json!({"connection": id, "opening": opening, "warm_first": first, "samples": samples, "cleanup": cleanup})
}

fn fresh_connection(
    fixture: &Fixture,
    case: &Case,
    id: usize,
    instrumented: bool,
    failures: &mut Vec<String>,
) -> JsonValue {
    let (connection, opening) = match open_context(&fixture.database) {
        Ok(context) => context,
        Err(error) => return structural_error(case, "fresh open", error, failures),
    };
    let environment = if instrumented {
        match sqlite_environment(&connection) {
            Ok(environment) => environment,
            Err(error) => structural_error(case, "SQLite environment", error, failures),
        }
    } else {
        JsonValue::Null
    };
    let sample = sample_or_error(&connection, fixture, case, instrumented, failures);
    let cleanup = close_context(&connection, case, failures);
    json!({"connection": id, "opening": opening, "first_query": sample, "sqlite": environment, "cleanup": cleanup})
}

fn structural_error(
    case: &Case,
    phase: &str,
    error: Box<dyn std::error::Error>,
    failures: &mut Vec<String>,
) -> JsonValue {
    failures.push(format!("{} {phase}: {error}", case.name));
    json!({"status": "error", "phase": phase, "error": error.to_string()})
}

fn close_context(connection: &Connection, case: &Case, failures: &mut Vec<String>) -> JsonValue {
    match connection.execute_batch("ROLLBACK") {
        Ok(()) => json!({"status": "ok"}),
        Err(error) => {
            structural_error(case, "read transaction ROLLBACK", Box::new(error), failures)
        }
    }
}

fn summarize(samples: &[&JsonValue], case_failures: usize) -> JsonValue {
    let failures = samples
        .iter()
        .filter(|sample| sample["status"] != "ok")
        .count();
    let mut metrics = BTreeMap::new();
    for name in [
        "prepare_us",
        "first_row_us",
        "consume_us",
        "total_us",
        "prepare_cpu_us",
        "first_row_cpu_us",
        "consume_cpu_us",
        "total_cpu_us",
    ] {
        let mut values = samples
            .iter()
            .filter_map(|sample| sample[name].as_u64())
            .collect::<Vec<_>>();
        values.sort_unstable();
        let percentile = |percentage: usize| {
            values
                .get(
                    values
                        .len()
                        .saturating_mul(percentage)
                        .div_ceil(100)
                        .saturating_sub(1),
                )
                .copied()
        };
        metrics.insert(name, json!({"samples": values.len(), "p50": percentile(50), "max": values.last(),
                                   "p95": (values.len() >= 100 && case_failures == 0).then(|| percentile(95)).flatten()}));
    }
    json!({"attempts": samples.len(), "timed_sample_failures": failures, "case_failures": case_failures, "microseconds": metrics,
           "p95_rule": "at least 100 complete successful samples and no case errors, including setup/first/cleanup; warm-first and instrumentation excluded"})
}

fn sample_or_error(
    connection: &Connection,
    fixture: &Fixture,
    case: &Case,
    instrumented: bool,
    failures: &mut Vec<String>,
) -> JsonValue {
    performance::reset();
    performance::set_enabled(instrumented);
    let started = Instant::now();
    let result = sample(connection, fixture, case, instrumented);
    performance::set_enabled(false);
    match result {
        Ok(sample) => sample,
        Err(error) => {
            failures.push(format!("{}: {error}", case.name));
            json!({"status": "error", "error": error.to_string(), "elapsed_us": started.elapsed().as_micros(),
                   "instrumented": instrumented, "counters_debug": format!("{:?}", performance::snapshot())})
        }
    }
}

fn sample(
    connection: &Connection,
    fixture: &Fixture,
    case: &Case,
    instrumented: bool,
) -> Result<JsonValue> {
    let mut options = ExecutionOptions::default();
    options.snapshot = SnapshotSelector::Commit(
        fixture
            .commit
            .strip_prefix("commit/")
            .unwrap_or(&fixture.commit)
            .to_owned(),
    );
    let cpu_started = process_cpu_nanos();
    let started = Instant::now();
    let prepared = prepare(connection, &case.query, BTreeMap::new(), options)?;
    let prepare_us = started.elapsed().as_micros();
    let prepare_cpu_us = elapsed_cpu_micros(cpu_started);
    if prepared.columns != case.columns {
        return Err(format!(
            "columns differ: {:?} / {:?}",
            prepared.columns, case.columns
        )
        .into());
    }
    let plan = instrumented.then(|| prepared.physical.explain());
    let resources = prepared.resources.clone();
    let consumed = consume(connection, prepared)?;
    let first_row_us = consumed
        .first_row
        .then_some(prepare_us + consumed.first_pull_us);
    let first_row_cpu_us = consumed
        .first_row
        .then_some(sum_cpu_micros(prepare_cpu_us, consumed.first_pull_cpu_us))
        .flatten();
    let total_cpu_us = sum_cpu_micros(prepare_cpu_us, consumed.elapsed_cpu_us);
    let used = resources.used();
    if used != 0 {
        return Err(format!("query resources still retain {used} bytes after close").into());
    }
    let rows = encoded_rows(&consumed.rows);
    if bag(&rows)? != bag(&case.oracle)? {
        return Err(format!("complete result bag differs: {rows:?} / {:?}", case.oracle).into());
    }
    let encoded = serde_json::to_vec(&bag(&rows)?)?;
    let counters = instrumented.then(counter_report).transpose()?;
    Ok(
        json!({"status": "ok", "prepare_us": prepare_us, "first_pull_us": consumed.first_pull_us,
              "first_row_us": first_row_us, "consume_us": consumed.elapsed_us, "total_us": prepare_us + consumed.elapsed_us,
              "prepare_cpu_us": prepare_cpu_us, "first_pull_cpu_us": consumed.first_pull_cpu_us,
              "first_row_cpu_us": first_row_cpu_us, "consume_cpu_us": consumed.elapsed_cpu_us, "total_cpu_us": total_cpu_us,
              "rows": rows.len(), "bag_blake3": blake3::hash(&encoded).to_hex().to_string(),
              "columns_verified": true, "complete_bag_verified": true,
              "resource_peak": resources.peak(), "resource_used_after_close": used,
              "db_hits": consumed.db_hits, "physical_plan": plan, "counters": counters,
              "instrumented_complete_rows": instrumented.then_some(rows)}),
    )
}

struct Consumed {
    rows: Vec<Vec<Value>>,
    first_row: bool,
    first_pull_us: u128,
    first_pull_cpu_us: Option<u128>,
    elapsed_us: u128,
    elapsed_cpu_us: Option<u128>,
    db_hits: u64,
}

fn consume(
    connection: &Connection,
    prepared: lithograph_core::query::PreparedQuery,
) -> Result<Consumed> {
    let cpu_started = process_cpu_nanos();
    let started = Instant::now();
    let mut cursor = QueryCursor::new(prepared);
    let first = cursor.next_batch(connection, 1)?;
    let first_pull_us = started.elapsed().as_micros();
    let first_pull_cpu_us = elapsed_cpu_micros(cpu_started);
    let first_row = !first.rows.is_empty();
    let mut rows = Vec::new();
    let mut done = append_batch(first, &mut rows)?;
    while !done {
        done = append_batch(cursor.next_batch(connection, 64)?, &mut rows)?;
    }
    let summary = cursor.complete(connection)?;
    let db_hits = summary.metrics.db_hits;
    drop(summary);
    drop(cursor);
    let elapsed_us = started.elapsed().as_micros();
    let elapsed_cpu_us = elapsed_cpu_micros(cpu_started);
    Ok(Consumed {
        rows,
        first_row,
        first_pull_us,
        first_pull_cpu_us,
        elapsed_us,
        elapsed_cpu_us,
        db_hits,
    })
}

fn append_batch(
    batch: lithograph_core::query::QueryBatch,
    rows: &mut Vec<Vec<Value>>,
) -> Result<bool> {
    if rows.len().saturating_add(batch.rows.len()) > 64 {
        return Err("query exceeded the small-result harness bound of 64 complete rows".into());
    }
    rows.extend(batch.rows);
    Ok(batch.done)
}

fn encoded_rows(rows: &[Vec<Value>]) -> Vec<Vec<JsonValue>> {
    rows.iter()
        .map(|row| row.iter().map(encode_json).collect())
        .collect()
}

fn bag(rows: &[Vec<JsonValue>]) -> Result<BTreeMap<String, u64>> {
    let mut bag = BTreeMap::new();
    for row in rows {
        *bag.entry(serde_json::to_string(row)?).or_insert(0) += 1;
    }
    Ok(bag)
}

fn counter_report() -> Result<JsonValue> {
    // Derived Debug fields are all u64. Parsing the complete record also lets
    // this same source link the exact retained S3 rlib, before join_* existed.
    let raw = format!("{:?}", performance::snapshot());
    let body = raw
        .strip_prefix("PerformanceCounters {")
        .and_then(|s| s.strip_suffix('}'))
        .ok_or("unexpected counter Debug representation")?;
    let mut fields = BTreeMap::new();
    for field in body.split(',') {
        let (name, value) = field
            .trim()
            .split_once(':')
            .ok_or("malformed counter field")?;
        if fields
            .insert(name.trim(), value.trim().parse::<u64>()?)
            .is_some()
        {
            return Err("duplicate counter field".into());
        }
    }
    Ok(json!({"values": fields, "debug": raw}))
}

fn sqlite_environment(connection: &Connection) -> Result<JsonValue> {
    let (version, source): (String, String) =
        connection.query_row("SELECT sqlite_version(),sqlite_source_id()", [], |row| {
            Ok((row.get(0)?, row.get(1)?))
        })?;
    let mut statement = connection.prepare("PRAGMA compile_options")?;
    let options = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<std::result::Result<Vec<_>, _>>()?;
    Ok(json!({"version": version, "source_id": source, "compile_options": options}))
}
