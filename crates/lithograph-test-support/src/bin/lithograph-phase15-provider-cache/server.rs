use serde_json::{Value, json};
use std::error::Error;
use std::io::{ErrorKind, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

type ServerHandle = JoinHandle<Result<Vec<String>, String>>;
const MAX_REQUEST_BYTES: usize = 64 * 1024;

pub(super) struct EmbeddingServer {
    base_url: String,
    stop: Arc<AtomicBool>,
    handle: Option<ServerHandle>,
}

impl EmbeddingServer {
    pub(super) fn start(stages: &[usize]) -> Result<Self, Box<dyn Error>> {
        if stages.contains(&0) {
            return Err("embedding server stages must contain positive request counts".into());
        }
        let listener = TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let base_url = format!("http://{}/v1", listener.local_addr()?);
        let stop = Arc::new(AtomicBool::new(false));
        let worker_stop = Arc::clone(&stop);
        let stages = stages.to_vec();
        let handle = thread::spawn(move || serve(listener, &stages, &worker_stop));
        Ok(Self {
            base_url,
            stop,
            handle: Some(handle),
        })
    }

    pub(super) fn base_url(&self) -> &str {
        &self.base_url
    }

    pub(super) fn finish(mut self) -> Result<Vec<String>, Box<dyn Error>> {
        self.stop.store(true, Ordering::Relaxed);
        self.handle
            .take()
            .ok_or("embedding server was already collected")?
            .join()
            .map_err(|_| "embedding server panicked")?
            .map_err(|error| format!("embedding server failed: {error}").into())
    }
}

impl Drop for EmbeddingServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);
        if let Some(handle) = self.handle.take() {
            let _ = handle.join();
        }
    }
}

fn serve(
    listener: TcpListener,
    stages: &[usize],
    stop: &AtomicBool,
) -> Result<Vec<String>, String> {
    let mut inputs = Vec::new();
    for (stage, expected) in stages.iter().copied().enumerate() {
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut pending = Vec::with_capacity(expected);
        while pending.len() < expected {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream
                        .set_nonblocking(false)
                        .map_err(|error| error.to_string())?;
                    let input = read_request(&mut stream, deadline)?;
                    inputs.push(input.clone());
                    pending.push((stream, input));
                }
                Err(error) if error.kind() == ErrorKind::WouldBlock => {
                    if stop.load(Ordering::Relaxed) {
                        return Ok(inputs);
                    }
                    if Instant::now() >= deadline {
                        return Err(format!(
                            "stage {stage} timed out: expected {expected} requests, got {}",
                            pending.len()
                        ));
                    }
                    thread::sleep(Duration::from_millis(2));
                }
                Err(error) => return Err(format!("HTTP accept failed: {error}")),
            }
        }
        for (mut stream, input) in pending {
            respond(&mut stream, &input, deadline)?;
        }
    }
    Ok(inputs)
}

fn read_request(stream: &mut TcpStream, deadline: Instant) -> Result<String, String> {
    let mut bytes = Vec::new();
    let mut buffer = [0_u8; 4096];
    let mut body = None;
    loop {
        stream
            .set_read_timeout(Some(operation_timeout(deadline)?))
            .map_err(|error| error.to_string())?;
        let count = stream
            .read(&mut buffer)
            .map_err(|error| format!("HTTP request read failed: {error}"))?;
        if count == 0 {
            return Err("HTTP request ended before its declared body".to_owned());
        }
        bytes.extend_from_slice(&buffer[..count]);
        if bytes.len() > MAX_REQUEST_BYTES {
            return Err("HTTP request exceeds the fixture size limit".to_owned());
        }
        if body.is_none()
            && let Some(position) = bytes.windows(4).position(|window| window == b"\r\n\r\n")
        {
            let start = position + 4;
            let headers = std::str::from_utf8(&bytes[..start])
                .map_err(|error| format!("HTTP headers are not UTF-8: {error}"))?;
            let length = content_length(headers)?;
            let end = start
                .checked_add(length)
                .filter(|end| *end <= MAX_REQUEST_BYTES)
                .ok_or("HTTP request exceeds the fixture size limit")?;
            body = Some((start, end));
        }
        if let Some((start, end)) = body
            && bytes.len() >= end
        {
            let request: Value = serde_json::from_slice(&bytes[start..end])
                .map_err(|error| format!("invalid embedding request JSON: {error}"))?;
            return match request.get("input").and_then(Value::as_array) {
                Some(inputs) if inputs.len() == 1 => inputs[0]
                    .as_str()
                    .map(str::to_owned)
                    .ok_or_else(|| "embedding input must contain a string".to_owned()),
                _ => Err("embedding input must be a single-string array".to_owned()),
            };
        }
    }
}

fn content_length(headers: &str) -> Result<usize, String> {
    let mut length = None;
    for line in headers.lines().skip(1) {
        if let Some((name, value)) = line.split_once(':')
            && name.eq_ignore_ascii_case("content-length")
        {
            if length.is_some() {
                return Err("HTTP request has duplicate Content-Length headers".to_owned());
            }
            length = Some(
                value
                    .trim()
                    .parse::<usize>()
                    .map_err(|error| format!("invalid HTTP Content-Length: {error}"))?,
            );
        }
    }
    length.ok_or_else(|| "HTTP request has no Content-Length header".to_owned())
}

fn respond(stream: &mut TcpStream, input: &str, deadline: Instant) -> Result<(), String> {
    let vector = match input {
        "shared" => [3.0, 4.0],
        "different" => [5.0, 6.0],
        _ => [1.0, 2.0],
    };
    let body = json!({"data": [{"index": 0, "embedding": vector}]}).to_string();
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    stream
        .set_write_timeout(Some(operation_timeout(deadline)?))
        .map_err(|error| error.to_string())?;
    stream
        .write_all(response.as_bytes())
        .map_err(|error| format!("HTTP response write failed: {error}"))
}

fn operation_timeout(deadline: Instant) -> Result<Duration, String> {
    let remaining = deadline.saturating_duration_since(Instant::now());
    if remaining.is_zero() {
        Err("embedding server stage deadline expired".to_owned())
    } else {
        Ok(remaining.min(Duration::from_secs(2)))
    }
}
