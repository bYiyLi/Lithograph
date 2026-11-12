//! S22.4 Core timing and counter probes over the retained, small-result fixtures.

#[path = "phase22_join_work/config.rs"]
mod config;
#[path = "phase22_join_work/fixture.rs"]
mod fixture;
#[path = "phase22_join_work/measure.rs"]
mod measure;

use std::error::Error;
use std::fs::{self, File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use serde_json::{Value as JsonValue, json};

type Result<T> = std::result::Result<T, Box<dyn Error>>;

fn main() -> Result<()> {
    let config = config::Config::parse()?;
    let fixture = fixture::Fixture::load(&config.manifest, config.case.as_deref())?;
    let provenance = provenance(&config)?;
    let cpu_clock = measure::cpu_clock_metadata();
    let before = inspect_database(&fixture.database)?;
    let mut output = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&config.output)?;
    let mut failures = Vec::new();
    let mut cases = Vec::new();
    for case in &fixture.cases {
        cases.push(measure::run_case(&fixture, case, &config, &mut failures));
    }
    let after = match inspect_database(&fixture.database) {
        Ok(state) => state,
        Err(error) => {
            failures.push(format!("fixture final inspection: {error}"));
            json!({"status": "error", "error": error.to_string()})
        }
    };
    if before != after {
        failures.push("fixture file or Branch head changed".to_owned());
    }
    let report = json!({
        "format": "lithograph-phase22-join-core-v1", "adapter": "Core",
        "mode": config.mode.name(), "manifest": config.manifest,
        "database": fixture.database, "snapshot": {"at": fixture.commit},
        "timing_unit": "microseconds", "clock": "std::time::Instant (wall time)",
        "timing_boundary": "prepare includes parse/plan/snapshot; consume includes cursor construction, first pull, complete and drop; oracle/report encoding excluded",
        "cpu_clock": cpu_clock,
        "cpu_and_process_rss": "per-sample CPU measured independently; external process CPU/RSS includes setup and is reported separately",
        "read_context": "read-only main connection; one caller-owned DEFERRED transaction per connection spans prepare and consume, including all warm iterations",
        "cache_class": "main file fully hashed before timing: OS file-cache pre-read; warm or independent fresh first query; no OS cache purge",
        "instrumentation": "disabled during timing; separate fresh counter probe per case",
        "fixture_before": before, "fixture_after": after,
        "provenance": provenance, "cases": cases, "failures": failures,
    });
    serde_json::to_writer_pretty(&mut output, &report)?;
    output.write_all(b"\n")?;
    output.sync_all()?;
    if !failures.is_empty() {
        return Err(format!(
            "{} Core Join checks failed; see {}",
            failures.len(),
            config.output.display()
        )
        .into());
    }
    println!(
        "{}",
        json!({"output": config.output, "cases": cases.len(), "failures": 0})
    );
    Ok(())
}

pub(crate) fn file_digest(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hash = blake3::Hasher::new();
    let mut buffer = [0_u8; 16 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            return Ok(hash.finalize().to_hex().to_string());
        }
        hash.update(&buffer[..read]);
    }
}

pub(crate) fn read_json(path: &Path) -> Result<JsonValue> {
    if fs::metadata(path)?.len() > 256 * 1024 {
        return Err("fixture/provenance JSON exceeds the 256 KiB harness bound".into());
    }
    Ok(serde_json::from_reader(File::open(path)?)?)
}

fn inspect_database(path: &Path) -> Result<JsonValue> {
    let connection = measure::open_read_only(path)?.0;
    let head = lithograph_core::storage::branch_head(&connection, "main")?.to_hex();
    let mut sidecars = Vec::new();
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = std::path::PathBuf::from(format!("{}{suffix}", path.display()));
        if sidecar.exists() {
            sidecars.push(json!({"suffix": suffix, "bytes": fs::metadata(sidecar)?.len()}));
        }
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
            Ok::<_, Box<dyn Error>>(json!({"path": path, "file_blake3": file_digest(path)?,
                                      "record": read_json(path)?}))
        })
        .transpose()?;
    Ok(
        json!({"executable": executable, "executable_blake3": file_digest(&executable)?,
              "os": std::env::consts::OS, "architecture": std::env::consts::ARCH,
              "supplied_source_and_link_record": supplied}),
    )
}
