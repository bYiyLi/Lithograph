#![forbid(unsafe_code)]

use lithograph_test_support::sqlite::{
    FileDatabaseFixture, execute_script, extension_load_command,
};
use rusqlite::Connection;
use serde::Serialize;
use serde_json::json;
use std::error::Error;
use std::fs;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::thread::{self, JoinHandle};
use std::time::Duration;

const META_TABLE: &str = "_openai_compatible_cache_meta";
const ENTRY_TABLE: &str = "_openai_compatible_embedding_entries";
type EmbeddingServerHandle = JoinHandle<Result<usize, String>>;
type EmbeddingServer = (String, EmbeddingServerHandle);

#[derive(Serialize)]
struct ProbeResult {
    phase: &'static str,
    provider: String,
    probe: String,
    checks: Vec<&'static str>,
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
    let provider = required_path_arg(1, "OpenAI-compatible Provider")?;
    let probe = required_path_arg(2, "Provider cache probe extension")?;
    let provider_load = load_command(&provider, "sqlite3_lithographopenaicompatible_init")?;
    let probe_load = load_command(&probe, "sqlite3_providercacheprobe_init")?;
    let mut checks = Vec::new();

    check_reopen_budget_and_secret_storage(&provider_load, &probe_load)?;
    checks.push("provider-cache-reopen-fifo-budget-secret-safe");
    check_oversized_entry(&provider_load, &probe_load)?;
    checks.push("provider-cache-oversized-entry");
    check_corrupt_entry_recovery(&provider_load, &probe_load)?;
    checks.push("provider-cache-corrupt-entry-recovery");
    check_marker_and_accounting_fail_closed(&provider_load, &probe_load)?;
    checks.push("provider-cache-marker-accounting-fail-closed");
    check_same_main_rejected(&provider_load, &probe_load)?;
    checks.push("provider-cache-same-main-rejected");
    check_busy_retry_cancellation(&provider_load, &probe_load)?;
    checks.push("provider-cache-busy-cancellation");
    check_concurrent_publish(&provider_load, &probe_load)?;
    checks.push("provider-cache-multi-process-concurrent-publish");

    Ok(ProbeResult {
        phase: "15-provider-owned-cache",
        provider: provider.display().to_string(),
        probe: probe.display().to_string(),
        checks,
    })
}

fn check_reopen_budget_and_secret_storage(
    provider_load: &str,
    probe_load: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1505_0001)?;
    let cache = fixture.directory().join("provider-cache.db");
    let (base_url, server) = embedding_server(2)?;
    let config = provider_config(&base_url, &cache, 8);

    require_status(&fixture, provider_load, probe_load, &config, "alpha", 0)?;
    require_status(&fixture, provider_load, probe_load, &config, "beta", 0)?;
    require_server_requests(server, 2)?;
    require_cache_state(&cache, 1, 8)?;

    require_status(&fixture, provider_load, probe_load, &config, "beta", 0)?;

    let smaller = provider_config(&base_url, &cache, 4);
    require_status(&fixture, provider_load, probe_load, &smaller, "beta", 2)?;
    require_cache_state(&cache, 0, 0)?;

    let bytes = fs::read(&cache)?;
    for forbidden in [
        b"phase15-secret".as_slice(),
        b"alpha".as_slice(),
        b"beta".as_slice(),
    ] {
        require(
            !bytes
                .windows(forbidden.len())
                .any(|window| window == forbidden),
            "cache database leaked a raw secret or embedding input",
        )?;
    }
    Ok(())
}

fn check_oversized_entry(provider_load: &str, probe_load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1505_0002)?;
    let cache = fixture.directory().join("provider-cache.db");
    let (base_url, server) = embedding_server(1)?;
    let config = provider_config(&base_url, &cache, 4);
    require_status(&fixture, provider_load, probe_load, &config, "oversized", 0)?;
    require_server_requests(server, 1)?;
    require_cache_state(&cache, 0, 0)
}

fn check_corrupt_entry_recovery(
    provider_load: &str,
    probe_load: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1505_0003)?;
    let cache = fixture.directory().join("provider-cache.db");
    let (base_url, server) = embedding_server(2)?;
    let config = provider_config(&base_url, &cache, 8);

    require_status(&fixture, provider_load, probe_load, &config, "repair-me", 0)?;
    execute_script(
        &cache,
        &format!("UPDATE {ENTRY_TABLE} SET vector_blob=X'0000';"),
    )?;
    require_status(&fixture, provider_load, probe_load, &config, "repair-me", 0)?;
    require_server_requests(server, 2)?;
    require_cache_state(&cache, 1, 8)
}

fn check_marker_and_accounting_fail_closed(
    provider_load: &str,
    probe_load: &str,
) -> Result<(), Box<dyn Error>> {
    let marker_fixture = FileDatabaseFixture::new(0x1505_0004)?;
    let marker_cache = marker_fixture.directory().join("provider-cache.db");
    let (marker_url, marker_server) = embedding_server(1)?;
    let marker_config = provider_config(&marker_url, &marker_cache, 8);
    require_status(
        &marker_fixture,
        provider_load,
        probe_load,
        &marker_config,
        "marker",
        0,
    )?;
    require_server_requests(marker_server, 1)?;
    execute_script(
        &marker_cache,
        &format!("UPDATE {META_TABLE} SET magic='wrong-cache-kind' WHERE id=1;"),
    )?;
    require_status(
        &marker_fixture,
        provider_load,
        probe_load,
        &marker_config,
        "marker",
        1,
    )?;

    let counter_fixture = FileDatabaseFixture::new(0x1505_0005)?;
    let counter_cache = counter_fixture.directory().join("provider-cache.db");
    let (counter_url, counter_server) = embedding_server(1)?;
    let counter_config = provider_config(&counter_url, &counter_cache, 8);
    require_status(
        &counter_fixture,
        provider_load,
        probe_load,
        &counter_config,
        "counter",
        0,
    )?;
    require_server_requests(counter_server, 1)?;
    execute_script(
        &counter_cache,
        &format!(
            "PRAGMA ignore_check_constraints=ON;\
             UPDATE {META_TABLE} SET used_payload_bytes=8,used_payload_bytes_check=0 WHERE id=1;"
        ),
    )?;
    require_status(
        &counter_fixture,
        provider_load,
        probe_load,
        &counter_config,
        "counter",
        2,
    )
}

fn check_same_main_rejected(provider_load: &str, probe_load: &str) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1505_0006)?;
    let config = provider_config("http://127.0.0.1:9/v1", fixture.path(), 8);
    require_status(&fixture, provider_load, probe_load, &config, "same-main", 1)?;
    let output = fixture.execute_script(&format!(
        "SELECT 'objects=' || count(*) FROM sqlite_schema \
         WHERE name IN ('{META_TABLE}','{ENTRY_TABLE}');"
    ))?;
    require(
        output.lines().any(|line| line.trim() == "objects=0"),
        "same-main rejection created Provider cache tables in the host database",
    )
}

fn check_busy_retry_cancellation(
    provider_load: &str,
    probe_load: &str,
) -> Result<(), Box<dyn Error>> {
    let fixture = FileDatabaseFixture::new(0x1505_0007)?;
    let cache = fixture.directory().join("provider-cache.db");
    let (base_url, server) = embedding_server(1)?;
    let config = provider_config(&base_url, &cache, 8);
    require_status(&fixture, provider_load, probe_load, &config, "cancel-me", 0)?;
    require_server_requests(server, 1)?;
    execute_script(
        &cache,
        &format!("UPDATE {ENTRY_TABLE} SET vector_blob=X'0000';"),
    )?;

    let locker = Connection::open(&cache)?;
    locker.execute_batch("BEGIN IMMEDIATE")?;
    let output = fixture.execute_script(&format!(
        "{provider_load}\n{probe_load}\n\
         SELECT 'status=' || provider_cache_probe_status_cancel({config}, 'cancel-me', 2, 8);",
        config = sql_literal(&config),
    ))?;
    let status = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("status="))
        .ok_or_else(|| format!("missing cancelled provider status in {output:?}"))?
        .parse::<i32>()?;
    locker.execute_batch("ROLLBACK")?;
    require(
        status == 4,
        &format!("busy cache operation returned status {status}, expected CANCELLED=4"),
    )?;
    require_cache_state(&cache, 1, 8)
}

fn check_concurrent_publish(provider_load: &str, probe_load: &str) -> Result<(), Box<dyn Error>> {
    let seed_fixture = FileDatabaseFixture::new(0x1505_0008)?;
    let cache = seed_fixture.directory().join("provider-cache.db");
    let (seed_url, seed_server) = embedding_server(1)?;
    let seed_config = provider_config(&seed_url, &cache, 24);
    require_status(
        &seed_fixture,
        provider_load,
        probe_load,
        &seed_config,
        "seed",
        0,
    )?;
    require_server_requests(seed_server, 1)?;

    let (base_url, server) = embedding_barrier_server(2)?;
    let config = provider_config(&base_url, &cache, 24);
    let first_fixture = FileDatabaseFixture::new(0x1505_0009)?;
    let second_fixture = FileDatabaseFixture::new(0x1505_000a)?;
    let first_path = first_fixture.path().to_path_buf();
    let second_path = second_fixture.path().to_path_buf();
    let first_script = format!(
        "{provider_load}\n{probe_load}\n\
         SELECT 'status=' || provider_cache_probe_status({config}, 'concurrent-a', 2);",
        config = sql_literal(&config),
    );
    let second_script = format!(
        "{provider_load}\n{probe_load}\n\
         SELECT 'status=' || provider_cache_probe_status({config}, 'concurrent-b', 2);",
        config = sql_literal(&config),
    );
    let first = thread::spawn(move || {
        execute_script(&first_path, &first_script).map_err(|error| error.to_string())
    });
    let second = thread::spawn(move || {
        execute_script(&second_path, &second_script).map_err(|error| error.to_string())
    });
    let first_output = first
        .join()
        .map_err(|_| "first concurrent provider process panicked")?
        .map_err(|error| format!("first concurrent provider process failed: {error}"))?;
    let second_output = second
        .join()
        .map_err(|_| "second concurrent provider process panicked")?
        .map_err(|error| format!("second concurrent provider process failed: {error}"))?;
    require_server_requests(server, 2)?;
    for output in [&first_output, &second_output] {
        require(
            output.lines().any(|line| line.trim() == "status=0"),
            &format!("concurrent Provider publish failed: {output:?}"),
        )?;
    }
    require_cache_state(&cache, 3, 24)
}

fn require_status(
    fixture: &FileDatabaseFixture,
    provider_load: &str,
    probe_load: &str,
    config: &str,
    text: &str,
    expected: i32,
) -> Result<(), Box<dyn Error>> {
    let output = fixture.execute_script(&format!(
        "{provider_load}\n{probe_load}\n\
         SELECT 'status=' || provider_cache_probe_status({config}, {text}, 2);",
        config = sql_literal(config),
        text = sql_literal(text),
    ))?;
    let actual = output
        .lines()
        .find_map(|line| line.trim().strip_prefix("status="))
        .ok_or_else(|| format!("missing provider status in {output:?}"))?
        .parse::<i32>()?;
    if actual != expected {
        let diagnostic = fixture.execute_script(&format!(
            "{provider_load}\n{probe_load}\n\
             SELECT 'error=' || provider_cache_probe_error({config}, {text}, 2);",
            config = sql_literal(config),
            text = sql_literal(text),
        ))?;
        return Err(format!(
            "provider status mismatch: expected {expected}, got {actual}; {diagnostic}"
        )
        .into());
    }
    Ok(())
}

fn require_cache_state(path: &Path, entries: i64, used: i64) -> Result<(), Box<dyn Error>> {
    let output = execute_script(
        path,
        &format!(
            "SELECT 'state=' || (SELECT count(*) FROM {ENTRY_TABLE}) || ':' || \
             (SELECT used_payload_bytes FROM {META_TABLE} WHERE id=1);"
        ),
    )?;
    let expected = format!("state={entries}:{used}");
    require(
        output.lines().any(|line| line.trim() == expected),
        &format!("expected {expected:?}, got {output:?}"),
    )
}

fn provider_config(base_url: &str, cache: &Path, max_bytes: u64) -> String {
    json!({
        "base_url": base_url,
        "api_key": "phase15-secret",
        "model": "phase15-model",
        "timeout_ms": 250,
        "max_retries": 0,
        "batch_size": 1,
        "cache": {
            "enabled": true,
            "path": cache.to_string_lossy(),
            "max_bytes": max_bytes
        }
    })
    .to_string()
}

fn embedding_server(expected_requests: usize) -> Result<EmbeddingServer, Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let handle = thread::spawn(move || {
        let mut served = 0usize;
        for _ in 0..expected_requests {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            read_http_request(&mut stream)?;
            let body = r#"{"data":[{"index":0,"embedding":[1.0,2.0]}]}"#;
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
                body.len(),
                body
            );
            stream
                .write_all(response.as_bytes())
                .map_err(|error| error.to_string())?;
            served = served.saturating_add(1);
        }
        Ok(served)
    });
    Ok((format!("http://{address}/v1"), handle))
}

fn embedding_barrier_server(expected_requests: usize) -> Result<EmbeddingServer, Box<dyn Error>> {
    let listener = TcpListener::bind("127.0.0.1:0")?;
    let address = listener.local_addr()?;
    let handle = thread::spawn(move || {
        let mut streams = Vec::with_capacity(expected_requests);
        for _ in 0..expected_requests {
            let (mut stream, _) = listener.accept().map_err(|error| error.to_string())?;
            read_http_request(&mut stream)?;
            streams.push(stream);
        }
        let body = r#"{"data":[{"index":0,"embedding":[1.0,2.0]}]}"#;
        let response = format!(
            "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body
        );
        for mut stream in streams {
            stream
                .write_all(response.as_bytes())
                .map_err(|error| error.to_string())?;
        }
        Ok(expected_requests)
    });
    Ok((format!("http://{address}/v1"), handle))
}

fn read_http_request(stream: &mut TcpStream) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_secs(2)))
        .map_err(|error| error.to_string())?;
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut header_end = None;
    let mut content_length = 0usize;
    loop {
        let count = stream
            .read(&mut buffer)
            .map_err(|error| error.to_string())?;
        if count == 0 {
            break;
        }
        bytes.extend_from_slice(&buffer[..count]);
        if header_end.is_none()
            && let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let end = position + 4;
            header_end = Some(end);
            let headers = String::from_utf8_lossy(&bytes[..end]);
            content_length = headers
                .lines()
                .find_map(|line| {
                    let (name, value) = line.split_once(':')?;
                    name.eq_ignore_ascii_case("content-length")
                        .then(|| value.trim().parse::<usize>().ok())
                        .flatten()
                })
                .unwrap_or(0);
        }
        if let Some(end) = header_end
            && bytes.len() >= end.saturating_add(content_length)
        {
            return Ok(());
        }
    }
    Err("HTTP request ended before its declared body".to_owned())
}

fn require_server_requests(
    handle: JoinHandle<Result<usize, String>>,
    expected: usize,
) -> Result<(), Box<dyn Error>> {
    let actual = handle
        .join()
        .map_err(|_| "synthetic embedding server panicked")?
        .map_err(|error| format!("synthetic embedding server failed: {error}"))?;
    require(
        actual == expected,
        &format!("expected {expected} HTTP requests, got {actual}"),
    )
}

fn required_path_arg(index: usize, role: &str) -> Result<PathBuf, Box<dyn Error>> {
    let value = std::env::args()
        .nth(index)
        .ok_or_else(|| format!("missing {role} argument"))?;
    Ok(fs::canonicalize(value)?)
}

fn load_command(path: &Path, entrypoint: &str) -> Result<String, Box<dyn Error>> {
    Ok(format!("{} {entrypoint}", extension_load_command(path)?))
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
