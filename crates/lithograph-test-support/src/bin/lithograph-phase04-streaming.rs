#![forbid(unsafe_code)]

use lithograph_core::storage::{
    CommitMetadata, LayerBuilder, allocate_node_id, branch_head, commit_layer, create_checkpoint,
};
use lithograph_test_support::sqlite::{FileDatabaseFixture, extension_load_command};
use rusqlite::Connection;
use serde::Serialize;
use std::env;
use std::error::Error;
use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};

const DEFAULT_ROWS: usize = 1_000_000;
const BASELINE_ROWS: usize = 10_000;
const CHUNK_ROWS: usize = 100_000;
const MAX_RSS_BYTES: u64 = 256 * 1024 * 1024;
const MAX_GROWTH_BYTES: u64 = 32 * 1024 * 1024;

#[derive(Serialize)]
struct ProbeResult {
    phase: &'static str,
    rows: usize,
    baseline_rows: usize,
    baseline_rss_bytes: u64,
    full_rss_bytes: u64,
    rss_growth_bytes: u64,
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
    let mut args = env::args().skip(1);
    let extension = args
        .next()
        .map(PathBuf::from)
        .ok_or("usage: lithograph-phase04-streaming <extension-path> [rows]")?;
    let extension = fs::canonicalize(extension)?;
    let rows = args
        .next()
        .map(|value| value.parse::<usize>())
        .transpose()?
        .unwrap_or(DEFAULT_ROWS);
    if rows < BASELINE_ROWS {
        return Err(format!("rows must be at least {BASELINE_ROWS}").into());
    }
    let load = extension_load_command(&extension)?;
    let fixture = build_fixture(&load, rows)?;
    let baseline_rows = BASELINE_ROWS.min(rows);
    let baseline_rss = measure_rows_query(fixture.path(), &load, Some(baseline_rows), rows)?;
    let full_rss = measure_rows_query(fixture.path(), &load, None, rows)?;
    let growth = full_rss.saturating_sub(baseline_rss);
    require(
        full_rss <= MAX_RSS_BYTES,
        &format!("full streaming RSS {full_rss} exceeds {MAX_RSS_BYTES}"),
    )?;
    require(
        growth <= MAX_GROWTH_BYTES,
        &format!("streaming RSS growth {growth} exceeds {MAX_GROWTH_BYTES}"),
    )?;
    Ok(ProbeResult {
        phase: "04-read-query-engine-streaming",
        rows,
        baseline_rows,
        baseline_rss_bytes: baseline_rss,
        full_rss_bytes: full_rss,
        rss_growth_bytes: growth,
    })
}

fn build_fixture(load: &str, rows: usize) -> Result<FileDatabaseFixture, Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x0404)?;
    fixture.execute_script(&format!("{load}\nSELECT lithograph_init();"))?;
    let connection = Connection::open(fixture.path())?;
    connection.execute_batch("PRAGMA synchronous = OFF")?;
    let mut head = branch_head(&connection, "main")?;
    let mut remaining = rows;
    let mut chunk_index = 0_i64;
    while remaining > 0 {
        let chunk = remaining.min(CHUNK_ROWS);
        connection.execute_batch("BEGIN IMMEDIATE")?;
        let mut layer = LayerBuilder::default();
        for _ in 0..chunk {
            layer.add_node(allocate_node_id(&connection)?)?;
        }
        let next = commit_layer(
            &connection,
            "main",
            head,
            None,
            &layer,
            &CommitMetadata {
                author: Some("phase04-scale".to_owned()),
                message: Some(format!("streaming fixture chunk {chunk_index}")),
                committed_at: chunk_index + 1,
            },
        )?;
        connection.execute_batch("COMMIT")?;
        head = next;
        remaining -= chunk;
        chunk_index += 1;
    }
    create_checkpoint(&connection, head)?;
    drop(connection);
    Ok(fixture)
}

fn measure_rows_query(
    database: &Path,
    load: &str,
    limit: Option<usize>,
    total_rows: usize,
) -> Result<u64, Box<dyn Error>> {
    let query = limit.map_or_else(
        || "MATCH (n) RETURN 1 AS one".to_owned(),
        |limit| format!("MATCH (n) RETURN 1 AS one LIMIT {limit}"),
    );
    let script = format!(
        "{load}\nSELECT count(*) FROM lithograph_rows('{}') WHERE event='row';\n",
        query.replace('\'', "''")
    );
    let sqlite = env::var_os("LITHOGRAPH_SQLITE3").unwrap_or_else(|| OsString::from("sqlite3"));
    let mut command = Command::new("/usr/bin/time");
    configure_time_command(&mut command)?;
    let mut child = command
        .arg(sqlite)
        .arg("-batch")
        .arg("-noheader")
        .arg(database)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()?;
    child
        .stdin
        .as_mut()
        .ok_or("sqlite3 stdin unavailable")?
        .write_all(script.as_bytes())?;
    drop(child.stdin.take());
    let output = child.wait_with_output()?;
    if !output.status.success() {
        return Err(format!(
            "streaming sqlite3 failed: {}",
            String::from_utf8_lossy(&output.stderr).trim()
        )
        .into());
    }
    let expected = limit.unwrap_or(total_rows);
    let actual = String::from_utf8_lossy(&output.stdout)
        .trim()
        .parse::<usize>()?;
    require(
        actual == expected,
        &format!("streaming query returned {actual} rows, expected {expected}"),
    )?;
    parse_max_rss(&String::from_utf8_lossy(&output.stderr))
}

#[cfg(target_os = "macos")]
fn configure_time_command(command: &mut Command) -> Result<(), Box<dyn Error>> {
    command.arg("-l");
    Ok(())
}

#[cfg(target_os = "linux")]
fn configure_time_command(command: &mut Command) -> Result<(), Box<dyn Error>> {
    command.arg("-v");
    Ok(())
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn configure_time_command(_command: &mut Command) -> Result<(), Box<dyn Error>> {
    Err("Phase 04 streaming RSS probe supports macOS and Linux".into())
}

#[cfg(target_os = "macos")]
fn parse_max_rss(stderr: &str) -> Result<u64, Box<dyn Error>> {
    stderr
        .lines()
        .find(|line| line.contains("maximum resident set size"))
        .and_then(|line| line.split_whitespace().next())
        .ok_or("missing maximum resident set size from /usr/bin/time")?
        .parse::<u64>()
        .map_err(Into::into)
}

#[cfg(target_os = "linux")]
fn parse_max_rss(stderr: &str) -> Result<u64, Box<dyn Error>> {
    let kib = stderr
        .lines()
        .find(|line| line.contains("Maximum resident set size (kbytes)"))
        .and_then(|line| line.split(':').nth(1))
        .map(str::trim)
        .ok_or("missing maximum resident set size from /usr/bin/time")?
        .parse::<u64>()?;
    Ok(kib.saturating_mul(1024))
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
fn parse_max_rss(_stderr: &str) -> Result<u64, Box<dyn Error>> {
    Err("Phase 04 streaming RSS probe supports macOS and Linux".into())
}

fn require(condition: bool, message: &str) -> Result<(), Box<dyn Error>> {
    if condition {
        Ok(())
    } else {
        Err(message.to_owned().into())
    }
}
