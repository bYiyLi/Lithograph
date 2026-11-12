//! S22.5 Core timing over exact-ID fixtures, without retaining large row sets.

#![recursion_limit = "256"]

#[path = "phase22_hash_work/config.rs"]
mod config;
#[path = "phase22_hash_work/fixture.rs"]
mod fixture;
#[path = "phase22_hash_work/measure.rs"]
mod measure;
#[path = "phase22_hash_work/oracle.rs"]
mod oracle;
#[path = "phase22_hash_work/profile.rs"]
mod profile;

use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use lithograph_core::storage::branch_head;
use rusqlite::{Connection, OpenFlags};
use serde_json::{Value as JsonValue, json};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() -> Result<()> {
    let config = config::Config::parse()?;
    let fixture = fixture::Fixture::load(&config.manifest, &config)?;
    let before = inspect_database(&fixture.database)?;
    if before["blake3"] != fixture.database_blake3 || before["bytes"] != fixture.database_bytes {
        return Err("actual fixture bytes/hash differ from its immutable manifest".into());
    }
    let provenance = provenance(&config)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config.output)?;
    let measured = measure::run(&fixture, &config);
    let after = inspect_database(&fixture.database);
    let unchanged = after.as_ref().is_ok_and(|value| value == &before);
    let passed = measured["status"] == "passed" && unchanged;
    let report = json!({
        "format": "lithograph-phase22-hash-core-v1", "surface": "Core",
        "status": if passed { "passed" } else { "failed" },
        "mode": config.mode.name(), "before_engine_measurement": config.before,
        "manifest": config.manifest, "case": config.case, "database": fixture.database,
        "snapshot": {"at": fixture.commit}, "nodes": fixture.nodes,
        "distribution": fixture.distribution, "hot_rows": fixture.hot_rows,
        "expected_rows": fixture.expected, "query": fixture.query, "columns": fixture.columns,
        "memory_limit_bytes": config.memory_limit,
        "sqlite_source_id_expected": config.sqlite_source_id,
        "sqlite_source_id_guard": config.sqlite_source_id.is_some(),
        "timing_unit": "microseconds",
        "timing_boundary": "prepare entry to cursor complete and drop; includes every actual row's typed ID/value/uniqueness check and BLAKE3 update; setup/environment, final oracle/report encoding and main transaction close excluded",
        "first_row_boundary": "first caller-visible batch of exactly one row",
        "cpu_clock": "process CPU via rustix CLOCK_PROCESS_CPUTIME_ID; absent on unsupported platforms; no overhead subtraction",
        "sample_protocol": "full small24: 5 connections x 20 warm samples plus 100 fresh; warm-first separate. single: one normal drain; probe: one independent instrumented drain. long: 3 fresh connection drains in one process, no 3-sample P95",
        "RSS_protocol": "per-child process RSS must be collected by external runner invoking single three times; long mode is not independent-process RSS evidence",
        "read_context": "actual SQLITE_OPEN_READ_ONLY, fixed Commit options.at, caller-owned BEGIN DEFERRED across prepare and drain, explicit rollback and strict connection close",
        "cache_class": "fixture fully BLAKE3-read before and after measurements, outside timers; OS cache pre-read, not OS cold",
        "fixture_before": before, "fixture_after": after.as_ref().ok(),
        "fixture_unchanged": unchanged, "fixture_after_error": after.err().map(|error| error.to_string()),
        "provenance": provenance, "measurement": measured,
        "before_rule": "--before changes harness expectations only; use an independently frozen Before engine/link record, never the current engine as a false baseline"
    });
    serde_json::to_writer_pretty(&mut output, &report)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    println!(
        "{}",
        json!({"output": config.output, "status": report["status"]})
    );
    if !passed {
        return Err(
            "Hash workload failed; retained all completed raw samples and error in output".into(),
        );
    }
    Ok(())
}

pub(crate) fn read_json(path: &Path) -> Result<JsonValue> {
    if fs::metadata(path)?.len() > 256 * 1024 {
        return Err("fixture/provenance JSON exceeds the 256 KiB harness bound".into());
    }
    Ok(serde_json::from_reader(File::open(path)?)?)
}

fn file_digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0; 16 * 1024];
    loop {
        let bytes = file.read(&mut buffer)?;
        if bytes == 0 {
            return Ok(hash.finalize().to_hex().to_string());
        }
        hash.update(&buffer[..bytes]);
    }
}

fn inspect_database(path: &Path) -> Result<JsonValue> {
    let connection = Connection::open_with_flags(path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    if !connection.is_readonly("main")? {
        return Err("fixture inspection main is writable".into());
    }
    let head = branch_head(&connection, "main")?.to_hex();
    connection.close().map_err(|(_, error)| error)?;
    let mut sidecars = Vec::new();
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        let bytes = if sidecar.exists() {
            fs::metadata(sidecar)?.len()
        } else {
            0
        };
        if suffix != "-shm" && bytes != 0 {
            return Err("fixture must have no live WAL or journal".into());
        }
        sidecars.push(json!({"suffix": suffix, "bytes": bytes}));
    }
    Ok(
        json!({"bytes": fs::metadata(path)?.len(), "blake3": file_digest(path)?,
        "main_head": head, "sidecars": sidecars}),
    )
}

fn provenance(config: &config::Config) -> Result<JsonValue> {
    let executable = std::env::current_exe()?;
    let supplied = config
        .provenance
        .as_ref()
        .map(|path| {
            Ok::<_, Box<dyn Error>>(
                json!({"path": path, "blake3": file_digest(path)?, "record": read_json(path)?}),
            )
        })
        .transpose()?;
    Ok(
        json!({"executable": executable, "executable_blake3": file_digest(&executable)?,
        "manifest_blake3": file_digest(&config.manifest)?, "os": std::env::consts::OS,
        "architecture": std::env::consts::ARCH, "supplied_source_and_link_record": supplied}),
    )
}
