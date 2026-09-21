use std::env::VarError;
use std::io::Read as _;
use std::thread;
use std::time::{Duration, SystemTime};

use base64::Engine as _;
use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD};
use serde::{Deserialize, Serialize};

use crate::config::{EncodingFormat, ProviderConfig};

const MAX_DIMENSIONS: usize = 4_096;
const MAX_OUTPUT_BYTES: usize = 256 * 1024 * 1024;
const MAX_RESPONSE_BYTES: usize = 384 * 1024 * 1024;
const MAX_RETRY_DELAY: Duration = Duration::from_secs(5);
const CANCEL_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FailureKind {
    InvalidConfig,
    Io,
    Resource,
    Cancelled,
    Internal,
}

#[derive(Debug)]
pub(crate) struct Failure {
    pub(crate) kind: FailureKind,
    pub(crate) message: String,
}

impl Failure {
    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::InvalidConfig,
            message: message.into(),
        }
    }

    pub(crate) fn io(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Io,
            message: message.into(),
        }
    }

    pub(crate) fn resource(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Resource,
            message: message.into(),
        }
    }

    pub(crate) fn cancelled() -> Self {
        Self {
            kind: FailureKind::Cancelled,
            message: "embedding request cancelled".to_owned(),
        }
    }

    pub(crate) fn internal(message: impl Into<String>) -> Self {
        Self {
            kind: FailureKind::Internal,
            message: message.into(),
        }
    }
}

#[derive(Serialize)]
struct EmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
    #[serde(skip_serializing_if = "Option::is_none")]
    dimensions: Option<usize>,
    encoding_format: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    user: Option<&'a str>,
}

#[derive(Deserialize)]
struct EmbeddingResponse {
    data: Vec<EmbeddingItem>,
}

#[derive(Deserialize)]
struct EmbeddingItem {
    index: usize,
    embedding: EmbeddingWire,
}

#[derive(Deserialize)]
#[serde(untagged)]
enum EmbeddingWire {
    Float(Vec<f64>),
    Base64(String),
}

pub(crate) struct Client;

impl Client {
    pub(crate) const fn new() -> Self {
        Self
    }

    pub(crate) fn embed(
        &self,
        config: &ProviderConfig,
        texts: &[String],
        dimensions: usize,
        is_cancelled: impl FnMut() -> bool,
    ) -> Result<Vec<f32>, Failure> {
        self.embed_impl(config, texts, dimensions, None, is_cancelled)
    }

    pub(crate) fn embed_with_headers(
        &self,
        config: &ProviderConfig,
        texts: &[String],
        dimensions: usize,
        headers: &[(String, String)],
        is_cancelled: impl FnMut() -> bool,
    ) -> Result<Vec<f32>, Failure> {
        self.embed_impl(config, texts, dimensions, Some(headers), is_cancelled)
    }

    fn embed_impl(
        &self,
        config: &ProviderConfig,
        texts: &[String],
        dimensions: usize,
        headers: Option<&[(String, String)]>,
        mut is_cancelled: impl FnMut() -> bool,
    ) -> Result<Vec<f32>, Failure> {
        validate_shape(texts.len(), dimensions)?;
        let value_count = texts
            .len()
            .checked_mul(dimensions)
            .ok_or_else(|| Failure::resource("embedding output size overflow"))?;
        let output_bytes = value_count
            .checked_mul(std::mem::size_of::<f32>())
            .ok_or_else(|| Failure::resource("embedding output size overflow"))?;
        if output_bytes > MAX_OUTPUT_BYTES {
            return Err(Failure::resource(
                "embedding batch output exceeds the provider memory budget",
            ));
        }

        let agent = config.agent();
        let mut all_values = Vec::with_capacity(value_count);
        for batch in texts.chunks(config.batch_size) {
            ensure_not_cancelled(&mut is_cancelled)?;
            let values = self.embed_one_batch(
                &agent,
                config,
                batch,
                dimensions,
                headers,
                &mut is_cancelled,
            )?;
            all_values.extend(values);
        }
        Ok(all_values)
    }

    fn embed_one_batch(
        &self,
        agent: &ureq::Agent,
        config: &ProviderConfig,
        texts: &[String],
        dimensions: usize,
        headers: Option<&[(String, String)]>,
        is_cancelled: &mut impl FnMut() -> bool,
    ) -> Result<Vec<f32>, Failure> {
        let request = EmbeddingRequest {
            model: &config.model,
            input: texts,
            dimensions: config.send_dimensions.then_some(dimensions),
            encoding_format: config.encoding_format.as_str(),
            user: config.user.as_deref(),
        };
        let body = serde_json::to_vec(&request).map_err(|error| {
            Failure::internal(format!("failed to encode embedding request JSON: {error}"))
        })?;

        let mut attempt = 0_u32;
        loop {
            ensure_not_cancelled(is_cancelled)?;
            match send(agent, config, headers, &body) {
                Ok(mut response) if response.status().is_success() => {
                    ensure_not_cancelled(is_cancelled)?;
                    return decode_response(
                        &mut response,
                        texts.len(),
                        dimensions,
                        config.encoding_format,
                    );
                }
                Ok(response) => {
                    ensure_not_cancelled(is_cancelled)?;
                    let status = response.status().as_u16();
                    if !retryable_status(status) || attempt >= config.max_retries {
                        return Err(Failure::io(format!(
                            "OpenAI-compatible embeddings endpoint returned HTTP {status}"
                        )));
                    }
                    attempt += 1;
                    let delay = retry_delay(&response, attempt);
                    interruptible_sleep(delay, is_cancelled)?;
                }
                Err(error) if error.kind == FailureKind::Io && attempt < config.max_retries => {
                    attempt += 1;
                    interruptible_sleep(backoff(attempt), is_cancelled)?;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

fn validate_shape(text_count: usize, dimensions: usize) -> Result<(), Failure> {
    if text_count == 0 {
        return Err(Failure::invalid(
            "embedding batch must contain at least one text",
        ));
    }
    if !(1..=MAX_DIMENSIONS).contains(&dimensions) {
        return Err(Failure::invalid(
            "dimensions must be an integer between 1 and 4096",
        ));
    }
    Ok(())
}

fn send(
    agent: &ureq::Agent,
    config: &ProviderConfig,
    resolved_headers: Option<&[(String, String)]>,
    body: &[u8],
) -> Result<ureq::http::Response<ureq::Body>, Failure> {
    let owned_headers;
    let headers = match resolved_headers {
        Some(headers) => headers,
        None => {
            owned_headers = config
                .final_headers(environment_value)
                .map_err(Failure::invalid)?;
            &owned_headers
        }
    };
    let mut request = agent.post(&config.embeddings_url());
    for (name, value) in headers {
        request = request.header(name, value);
    }
    request.send(body).map_err(|_| {
        Failure::io("OpenAI-compatible embeddings request failed before receiving a response")
    })
}

pub(crate) fn environment_value(name: &str) -> Result<Option<String>, String> {
    match std::env::var(name) {
        Ok(value) => Ok(Some(value)),
        Err(VarError::NotPresent) => Ok(None),
        Err(VarError::NotUnicode(_)) => Err(format!(
            "api_key_env variable {name:?} is not valid Unicode"
        )),
    }
}

fn retryable_status(status: u16) -> bool {
    status == 408 || status == 429 || (500..=599).contains(&status)
}

fn retry_delay(response: &ureq::http::Response<ureq::Body>, attempt: u32) -> Duration {
    response
        .headers()
        .get("retry-after")
        .and_then(|value| value.to_str().ok())
        .and_then(parse_retry_after)
        .map(|delay| delay.min(MAX_RETRY_DELAY))
        .unwrap_or_else(|| backoff(attempt))
}

fn parse_retry_after(value: &str) -> Option<Duration> {
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let when = httpdate::parse_http_date(value).ok()?;
    Some(
        when.duration_since(SystemTime::now())
            .unwrap_or(Duration::ZERO),
    )
}

fn backoff(attempt: u32) -> Duration {
    let shift = attempt.saturating_sub(1).min(4);
    Duration::from_millis(100_u64.saturating_mul(1_u64 << shift))
}

fn interruptible_sleep(
    delay: Duration,
    is_cancelled: &mut impl FnMut() -> bool,
) -> Result<(), Failure> {
    let mut remaining = delay;
    while !remaining.is_zero() {
        ensure_not_cancelled(is_cancelled)?;
        let step = remaining.min(CANCEL_POLL_INTERVAL);
        thread::sleep(step);
        remaining = remaining.saturating_sub(step);
    }
    ensure_not_cancelled(is_cancelled)
}

fn ensure_not_cancelled(is_cancelled: &mut impl FnMut() -> bool) -> Result<(), Failure> {
    if is_cancelled() {
        Err(Failure::cancelled())
    } else {
        Ok(())
    }
}

fn decode_response(
    response: &mut ureq::http::Response<ureq::Body>,
    expected_count: usize,
    dimensions: usize,
    format: EncodingFormat,
) -> Result<Vec<f32>, Failure> {
    let body = read_response_body(response, MAX_RESPONSE_BYTES)?;
    let decoded: EmbeddingResponse = serde_json::from_str(&body)
        .map_err(|error| Failure::io(format!("invalid embeddings response JSON: {error}")))?;
    if decoded.data.len() != expected_count {
        return Err(Failure::internal(format!(
            "embeddings response count {} does not match input count {expected_count}",
            decoded.data.len()
        )));
    }

    let mut slots = vec![None; expected_count];
    for item in decoded.data {
        if item.index >= expected_count || slots[item.index].is_some() {
            return Err(Failure::internal(
                "embeddings response contains an invalid or duplicate index",
            ));
        }
        let vector = decode_vector(item.embedding, dimensions, format)?;
        slots[item.index] = Some(vector);
    }
    flatten_slots(slots, dimensions)
}

fn read_response_body(
    response: &mut ureq::http::Response<ureq::Body>,
    max_bytes: usize,
) -> Result<String, Failure> {
    let limit = max_bytes
        .checked_add(1)
        .and_then(|value| u64::try_from(value).ok())
        .ok_or_else(|| Failure::resource("embedding response size limit overflow"))?;
    let reader = response.body_mut().with_config().limit(limit).reader();
    let mut reader = reader.take(limit);
    let mut body = String::new();
    reader
        .read_to_string(&mut body)
        .map_err(map_response_read_error)?;
    if body.len() > max_bytes {
        return Err(Failure::resource(
            "embeddings response exceeds the provider memory budget",
        ));
    }
    Ok(body)
}

fn map_response_read_error(error: std::io::Error) -> Failure {
    let body_limit = error
        .get_ref()
        .and_then(|source| source.downcast_ref::<ureq::Error>())
        .is_some_and(|source| matches!(source, ureq::Error::BodyExceedsLimit(_)));
    if body_limit {
        Failure::resource("embeddings response exceeds the provider memory budget")
    } else {
        Failure::io(format!("failed to read embeddings response: {error}"))
    }
}

fn decode_vector(
    wire: EmbeddingWire,
    dimensions: usize,
    format: EncodingFormat,
) -> Result<Vec<f32>, Failure> {
    match (format, wire) {
        (EncodingFormat::Float, EmbeddingWire::Float(values)) => {
            decode_float_vector(values, dimensions)
        }
        (EncodingFormat::Base64, EmbeddingWire::Base64(value)) => {
            decode_base64_vector(&value, dimensions)
        }
        (EncodingFormat::Float, EmbeddingWire::Base64(_)) => Err(Failure::internal(
            "embeddings response returned base64 for encoding_format=float",
        )),
        (EncodingFormat::Base64, EmbeddingWire::Float(_)) => Err(Failure::internal(
            "embeddings response returned numeric coordinates for encoding_format=base64",
        )),
    }
}

fn decode_float_vector(values: Vec<f64>, dimensions: usize) -> Result<Vec<f32>, Failure> {
    if values.len() != dimensions {
        return Err(dimension_error(values.len(), dimensions));
    }
    values
        .into_iter()
        .map(|value| {
            let value = value as f32;
            if value.is_finite() {
                Ok(value)
            } else {
                Err(Failure::internal(
                    "embedding response contains a non-finite FLOAT32 coordinate",
                ))
            }
        })
        .collect()
}

fn decode_base64_vector(value: &str, dimensions: usize) -> Result<Vec<f32>, Failure> {
    let bytes = STANDARD
        .decode(value)
        .or_else(|_| STANDARD_NO_PAD.decode(value))
        .map_err(|_| Failure::internal("embedding response contains invalid base64"))?;
    let expected = dimensions
        .checked_mul(std::mem::size_of::<f32>())
        .ok_or_else(|| Failure::resource("embedding byte size overflow"))?;
    if bytes.len() != expected {
        return Err(dimension_error(
            bytes.len() / std::mem::size_of::<f32>(),
            dimensions,
        ));
    }
    let mut vector = Vec::with_capacity(dimensions);
    for chunk in bytes.as_chunks::<4>().0 {
        let value = f32::from_le_bytes([chunk[0], chunk[1], chunk[2], chunk[3]]);
        if !value.is_finite() {
            return Err(Failure::internal(
                "embedding response contains a non-finite FLOAT32 coordinate",
            ));
        }
        vector.push(value);
    }
    Ok(vector)
}

fn dimension_error(actual: usize, expected: usize) -> Failure {
    Failure::internal(format!(
        "embedding dimension {actual} does not match requested dimension {expected}"
    ))
}

fn flatten_slots(slots: Vec<Option<Vec<f32>>>, dimensions: usize) -> Result<Vec<f32>, Failure> {
    let capacity = slots
        .len()
        .checked_mul(dimensions)
        .ok_or_else(|| Failure::resource("embedding output size overflow"))?;
    let mut values = Vec::with_capacity(capacity);
    for vector in slots {
        values.extend(
            vector
                .ok_or_else(|| Failure::internal("embeddings response omitted an input index"))?,
        );
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    use super::*;

    struct StubResponse {
        status: u16,
        extra_headers: &'static str,
        body: &'static str,
    }

    fn provider_config(base_url: String) -> ProviderConfig {
        ProviderConfig::parse(
            serde_json::to_vec(&serde_json::json!({
                "base_url": base_url,
                "model": "embed-model",
                "api_key": "secret",
                "timeout_ms": 2000,
                "max_retries": 0,
                "batch_size": 32
            }))
            .expect("config json")
            .as_slice(),
        )
        .expect("provider config")
    }

    fn start_server(
        responses: Vec<StubResponse>,
    ) -> (String, Arc<Mutex<Vec<String>>>, std::thread::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind stub server");
        let address = listener.local_addr().expect("stub address");
        let requests = Arc::new(Mutex::new(Vec::new()));
        let captured = Arc::clone(&requests);
        let handle = std::thread::spawn(move || {
            let mut responses = VecDeque::from(responses);
            while let Some(response) = responses.pop_front() {
                let (mut stream, _) = listener.accept().expect("accept stub request");
                let request = read_request(&mut stream);
                captured.lock().expect("request lock").push(request);
                write!(
                    stream,
                    "HTTP/1.1 {} Test\r\nContent-Type: application/json\r\n{}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
                    response.status,
                    response.extra_headers,
                    response.body.len(),
                    response.body
                )
                .expect("write stub response");
            }
        });
        (format!("http://{address}/v1"), requests, handle)
    }

    fn read_request(stream: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        let mut buffer = [0_u8; 4096];
        let header_end = loop {
            let read = stream.read(&mut buffer).expect("read request");
            assert!(read > 0, "request ended before headers");
            bytes.extend_from_slice(&buffer[..read]);
            if let Some(end) = find_header_end(&bytes) {
                break end;
            }
        };
        let headers = String::from_utf8(bytes[..header_end].to_vec()).expect("utf8 headers");
        let content_length = headers
            .lines()
            .find_map(|line| {
                let (name, value) = line.split_once(':')?;
                name.eq_ignore_ascii_case("content-length")
                    .then(|| value.trim().parse::<usize>().expect("content length"))
            })
            .unwrap_or(0);
        let body_start = header_end + 4;
        while bytes.len() < body_start + content_length {
            let read = stream.read(&mut buffer).expect("read request body");
            assert!(read > 0, "request body ended early");
            bytes.extend_from_slice(&buffer[..read]);
        }
        String::from_utf8(bytes).expect("utf8 request")
    }

    fn find_header_end(bytes: &[u8]) -> Option<usize> {
        bytes.windows(4).position(|window| window == b"\r\n\r\n")
    }

    fn ok(body: &'static str) -> StubResponse {
        StubResponse {
            status: 200,
            extra_headers: "",
            body,
        }
    }

    fn single_request(
        text: String,
        dimensions: usize,
        configure: impl FnOnce(&mut ProviderConfig),
    ) -> String {
        let (base_url, requests, handle) =
            start_server(vec![ok(r#"{"data":[{"index":0,"embedding":[1]}]}"#)]);
        let mut config = provider_config(base_url);
        configure(&mut config);
        Client::new()
            .embed(&config, &[text], dimensions, || false)
            .expect("embed");
        handle.join().expect("stub join");
        requests.lock().expect("requests").pop().expect("request")
    }

    fn assert_response_rejected(body: &'static str) {
        let (base_url, _requests, handle) = start_server(vec![ok(body)]);
        let config = provider_config(base_url);
        assert!(
            Client::new()
                .embed(&config, &["alpha".to_owned()], 1, || false)
                .is_err()
        );
        handle.join().expect("stub join");
    }

    #[test]
    fn sends_complete_request_shape_and_restores_index_order() {
        let (base_url, requests, handle) = start_server(vec![ok(
            r#"{"data":[{"index":1,"embedding":[4,5,6]},{"index":0,"embedding":[1,2,3]}]}"#,
        )]);
        let mut config = provider_config(base_url);
        config.user = Some("caller".to_owned());
        config.organization = Some("org".to_owned());
        config.project = Some("project".to_owned());
        config
            .headers
            .insert("X-Test".to_owned(), "custom".to_owned());
        let values = Client::new()
            .embed(&config, &["alpha".to_owned(), "beta".to_owned()], 3, || {
                false
            })
            .expect("embed");
        assert_eq!(values, vec![1.0, 2.0, 3.0, 4.0, 5.0, 6.0]);
        handle.join().expect("stub join");

        let request = requests.lock().expect("requests").pop().expect("request");
        assert!(request.starts_with("POST /v1/embeddings HTTP/1.1"));
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("authorization: bearer secret"));
        assert!(lower.contains("openai-organization: org"));
        assert!(lower.contains("openai-project: project"));
        assert!(lower.contains("x-test: custom"));
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let body: serde_json::Value = serde_json::from_str(body).expect("json body");
        assert_eq!(body["model"], "embed-model");
        assert_eq!(body["input"], serde_json::json!(["alpha", "beta"]));
        assert_eq!(body["dimensions"], 3);
        assert_eq!(body["encoding_format"], "float");
        assert_eq!(body["user"], "caller");
    }

    #[test]
    fn optional_dimensions_can_be_omitted_and_empty_text_is_preserved() {
        let request = single_request(String::new(), 1, |config| {
            config.send_dimensions = false;
        });
        let body = request.split("\r\n\r\n").nth(1).expect("request body");
        let body: serde_json::Value = serde_json::from_str(body).expect("json body");
        assert!(body.get("dimensions").is_none());
        assert_eq!(body["input"], serde_json::json!([""]));
    }

    #[test]
    fn decodes_base64_float32_payload() {
        let mut bytes = Vec::new();
        for value in [1.25_f32, -2.5_f32] {
            bytes.extend(value.to_le_bytes());
        }
        let encoded = STANDARD.encode(bytes);
        let body = format!(r#"{{"data":[{{"index":0,"embedding":"{encoded}"}}]}}"#);
        let body: &'static str = Box::leak(body.into_boxed_str());
        let (base_url, _requests, handle) = start_server(vec![ok(body)]);
        let mut config = provider_config(base_url);
        config.encoding_format = EncodingFormat::Base64;
        let values = Client::new()
            .embed(&config, &["alpha".to_owned()], 2, || false)
            .expect("base64 embed");
        handle.join().expect("stub join");
        assert_eq!(values, vec![1.25, -2.5]);
    }

    #[test]
    fn custom_authorization_overrides_direct_key() {
        let request = single_request("alpha".to_owned(), 1, |config| {
            config
                .headers
                .insert("Authorization".to_owned(), "Custom credential".to_owned());
        });
        let lower = request.to_ascii_lowercase();
        assert!(lower.contains("authorization: custom credential"));
        assert!(!lower.contains("bearer secret"));
    }

    #[test]
    fn retries_retryable_status_and_honors_zero_retry_after() {
        let (base_url, requests, handle) = start_server(vec![
            StubResponse {
                status: 429,
                extra_headers: "Retry-After: 0\r\n",
                body: "{}",
            },
            ok(r#"{"data":[{"index":0,"embedding":[1]}]}"#),
        ]);
        let mut config = provider_config(base_url);
        config.max_retries = 1;
        Client::new()
            .embed(&config, &["alpha".to_owned()], 1, || false)
            .expect("retry embed");
        handle.join().expect("stub join");
        assert_eq!(requests.lock().expect("requests").len(), 2);
    }

    #[test]
    fn expired_http_date_retry_after_is_immediately_ready() {
        assert_eq!(
            parse_retry_after("Thu, 01 Jan 1970 00:00:00 GMT"),
            Some(Duration::ZERO)
        );
    }

    #[test]
    fn does_not_retry_non_retryable_client_error() {
        let (base_url, requests, handle) = start_server(vec![StubResponse {
            status: 401,
            extra_headers: "",
            body: "{}",
        }]);
        let mut config = provider_config(base_url);
        config.max_retries = 2;
        assert!(
            Client::new()
                .embed(&config, &["alpha".to_owned()], 1, || false)
                .is_err()
        );
        handle.join().expect("stub join");
        assert_eq!(requests.lock().expect("requests").len(), 1);
    }

    #[test]
    fn does_not_follow_redirects_with_custom_auth_headers() {
        let (base_url, requests, handle) = start_server(vec![StubResponse {
            status: 302,
            extra_headers: "Location: http://127.0.0.1:9/credential-sink\r\n",
            body: "{}",
        }]);
        let mut config = provider_config(base_url);
        config
            .headers
            .insert("X-API-Key".to_owned(), "redirect-secret".to_owned());
        let error = Client::new()
            .embed(&config, &["alpha".to_owned()], 1, || false)
            .expect_err("redirect must not be followed");
        handle.join().expect("stub join");
        assert_eq!(error.kind, FailureKind::Io);
        assert!(error.message.contains("HTTP 302"));
        assert!(!error.message.contains("redirect-secret"));
        assert_eq!(requests.lock().expect("requests").len(), 1);
    }

    #[test]
    fn splits_requests_by_configured_batch_size() {
        let (base_url, requests, handle) = start_server(vec![
            ok(r#"{"data":[{"index":0,"embedding":[1]}]}"#),
            ok(r#"{"data":[{"index":0,"embedding":[2]}]}"#),
        ]);
        let mut config = provider_config(base_url);
        config.batch_size = 1;
        let values = Client::new()
            .embed(&config, &["alpha".to_owned(), "beta".to_owned()], 1, || {
                false
            })
            .expect("batched embed");
        handle.join().expect("stub join");
        assert_eq!(values, vec![1.0, 2.0]);
        assert_eq!(requests.lock().expect("requests").len(), 2);
    }

    #[test]
    fn rejects_invalid_response_shape_and_wire_format() {
        assert_response_rejected(r#"{"data":[{"index":0,"embedding":[1,2]}]}"#);
        assert_response_rejected(r#"{"data":[{"index":0,"embedding":"AAAAAA=="}]}"#);
    }

    #[test]
    fn response_body_limit_is_enforced_while_reading() {
        let (base_url, _requests, handle) = start_server(vec![ok(
            r#"{"data":[{"index":0,"embedding":[1]}],"padding":"0123456789"}"#,
        )]);
        let config = provider_config(base_url);
        let agent = config.agent();
        let mut response = send(&agent, &config, None, b"{}").expect("stub response");
        let error = read_response_body(&mut response, 16).expect_err("response limit");
        handle.join().expect("stub join");
        assert_eq!(error.kind, FailureKind::Resource);
    }

    #[test]
    fn cancellation_stops_before_network_io() {
        let config = provider_config("http://127.0.0.1:9/v1".to_owned());
        let error = Client::new()
            .embed(&config, &["alpha".to_owned()], 1, || true)
            .expect_err("cancelled");
        assert_eq!(error.kind, FailureKind::Cancelled);
    }

    #[test]
    fn invalid_generated_headers_fail_as_config_without_transport_retry() {
        let mut config = provider_config("http://127.0.0.1:9/v1".to_owned());
        config.api_key = Some("secret\r\ninjected".to_owned());
        config.max_retries = 4;
        let error = Client::new()
            .embed(&config, &["alpha".to_owned()], 1, || false)
            .expect_err("invalid generated header");
        assert_eq!(error.kind, FailureKind::InvalidConfig);
        assert!(!error.message.contains("secret"));
        assert!(!error.message.contains("injected"));
    }

    #[test]
    fn transport_errors_do_not_echo_direct_credentials() {
        let config = provider_config("http://127.0.0.1:9/v1".to_owned());
        let error = Client::new()
            .embed(&config, &["alpha".to_owned()], 1, || false)
            .expect_err("transport failure");
        assert!(!error.message.contains("secret"));
    }
}
