//! One plain and one diagnostic drain of a public-CREATE S22.7 identity bag.
//! No latency claims: test-support counters are sampled independently of SQL timing.

#[path = "phase22_factor_work/oracle.rs"]
mod oracle;

use std::collections::BTreeMap;
use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use lithograph_core::cypher::Value;
use lithograph_core::performance;
use lithograph_core::query::{ExecutionOptions, QueryCursor, SnapshotSelector, prepare};
use rusqlite::{Connection, OpenFlags};
use serde::Deserialize;
use serde_json::json;

type Result<T> = std::result::Result<T, Box<dyn Error>>;

#[derive(Deserialize)]
struct Fixture {
    format: String,
    database: String,
    pin: String,
    public_create_oracle: bool,
    cases: Vec<Case>,
}

#[derive(Deserialize)]
struct Case {
    name: String,
    query: String,
    columns: Vec<String>,
    params: BTreeMap<String, i64>,
    expected_rows: usize,
    oracle: oracle::Recipe,
}

fn main() -> Result<()> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    if args.len() != 3 {
        return Err("usage: lithograph-phase22-factor-work <manifest> <case> <new-output>".into());
    }
    let fixture = load_fixture(Path::new(&args[0]))?;
    let case = fixture
        .cases
        .iter()
        .find(|case| case.name == args[1])
        .ok_or("missing case")?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&args[2])?;
    let report = report(&fixture, case, Path::new(&args[0]))?;
    serde_json::to_writer_pretty(&mut output, &report)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    println!(
        "{}",
        json!({"output": args[2], "case": case.name, "rows": case.expected_rows})
    );
    Ok(())
}

fn load_fixture(path: &Path) -> Result<Fixture> {
    if fs::metadata(path)?.len() > 2 * 1024 * 1024 {
        return Err("fixture manifest exceeds diagnostic bound".into());
    }
    let fixture: Fixture = serde_json::from_reader(File::open(path)?)?;
    if fixture.format != "phase22-s7-native-fixture-v1" || !fixture.public_create_oracle {
        return Err("requires the independently checked public CREATE fixture".into());
    }
    Ok(fixture)
}

fn report(fixture: &Fixture, case: &Case, manifest: &Path) -> Result<serde_json::Value> {
    let before = digest(Path::new(&fixture.database))?;
    let plain = probe(fixture, case, false)?;
    let instrumented = probe(fixture, case, true)?;
    let after = digest(Path::new(&fixture.database))?;
    if before != after
        || plain["rows"] != instrumented["rows"]
        || plain["plan"] != instrumented["plan"]
    {
        return Err("fixture or query result/plan changed during diagnostic probing".into());
    }
    Ok(json!({
        "format": "phase22-factor-diagnostic-v1", "case": case.name,
        "manifest": manifest, "manifest_blake3": digest(manifest)?,
        "database": fixture.database, "database_blake3": before, "pin": fixture.pin,
        "executable_blake3": digest(&std::env::current_exe()?)?,
        "sampling": "one complete plain drain plus one instrumented drain; no latency distribution",
        "scope": "ordinary and controlled whole-BindingRow successful copies and reservation-footprint estimates; Boolean/null conversion attempts, not every expression or allocator byte",
        "plain": plain, "instrumented": instrumented,
        "complete_bag_verified": true, "fixture_unchanged": true,
    }))
}

struct Instrumentation;

impl Drop for Instrumentation {
    fn drop(&mut self) {
        performance::set_enabled(false);
    }
}

fn probe(fixture: &Fixture, case: &Case, enabled: bool) -> Result<serde_json::Value> {
    let mut witness = oracle::Witness::new(&case.oracle, case.expected_rows)?;
    let db = Connection::open_with_flags(&fixture.database, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    db.execute_batch("BEGIN DEFERRED")?;
    let head = lithograph_core::storage::branch_head(&db, "main")?.to_hex();
    if fixture.pin != format!("commit/{head}") {
        return Err("fixture Branch differs from recorded pin".into());
    }
    performance::reset();
    performance::set_enabled(enabled);
    let _instrumentation = Instrumentation;
    let mut options = ExecutionOptions::default();
    options.snapshot = SnapshotSelector::Commit(head);
    options.memory_limit_bytes = Some(64 * 1024 * 1024);
    let params = case
        .params
        .iter()
        .map(|(key, value)| (key.clone(), Value::Integer(*value)))
        .collect();
    let prepared = prepare(&db, &case.query, params, options)?;
    if prepared.columns != case.columns {
        return Err("visible columns differ from public oracle".into());
    }
    let plan = prepared.physical.explain();
    let resources = prepared.resources.clone();
    let mut cursor = QueryCursor::new(prepared);
    let (db_hits, commit) = drain(&db, &mut cursor, &mut witness)?;
    drop(cursor);
    performance::set_enabled(false);
    let counters = performance::snapshot();
    witness.finish()?;
    if resources.used() != 0 || commit.as_deref() != Some(fixture.pin.as_str()) {
        return Err("cursor retained resources or completed against another Commit".into());
    }
    db.execute_batch("ROLLBACK")?;
    Ok(json!({
        "rows": case.expected_rows, "plan": plan, "db_hits": db_hits,
        "resource_peak_bytes": resources.peak(), "resource_used_after_close": resources.used(),
        "instrumentation_enabled": enabled,
        "binding_row_clones": counters.binding_row_clones,
        "binding_row_clone_accounted_bytes": counters.binding_row_clone_accounted_bytes,
        "predicate_conversions": counters.predicate_conversions,
        "adjacency_pages": counters.adjacency_pages,
    }))
}

fn drain(
    db: &Connection,
    cursor: &mut QueryCursor,
    witness: &mut oracle::Witness<'_>,
) -> Result<(u64, Option<String>)> {
    loop {
        let batch = cursor.next_batch(db, 64)?;
        for row in &batch.rows {
            witness.accept(row)?;
        }
        if batch.done {
            break;
        }
    }
    let summary = cursor.complete(db)?;
    let db_hits = summary.metrics.db_hits;
    let commit = summary.commit.clone();
    Ok((db_hits, commit))
}

fn digest(path: &Path) -> Result<String> {
    let mut input = File::open(path)?;
    let mut hasher = blake3::Hasher::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let bytes = input.read(&mut buffer)?;
        if bytes == 0 {
            return Ok(hasher.finalize().to_hex().to_string());
        }
        hasher.update(&buffer[..bytes]);
    }
}
