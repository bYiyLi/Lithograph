use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde::Deserialize;
use ureq::Agent;
use ureq::http::Uri;
use ureq::http::header::{HeaderName, HeaderValue};

const DEFAULT_BASE_URL: &str = "https://api.openai.com/v1";
const DEFAULT_TIMEOUT_MS: u64 = 30_000;
const DEFAULT_MAX_RETRIES: u32 = 2;
const DEFAULT_BATCH_SIZE: usize = 32;
const MAX_TIMEOUT_MS: u64 = 600_000;
const MAX_RETRIES: u32 = 8;
pub(crate) const MAX_BATCH_SIZE: usize = 2_048;
const MAX_SEMANTIC_IDENTITY_BYTES: usize = 256;
const MAX_CUSTOM_HEADER_VALUE_BYTES: usize = 60 * 1024;
const MAX_FINAL_HEADER_BYTES: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub(crate) enum EncodingFormat {
    Float,
    Base64,
}

impl EncodingFormat {
    pub(crate) const fn as_str(self) -> &'static str {
        match self {
            Self::Float => "float",
            Self::Base64 => "base64",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProviderConfig {
    #[serde(default = "default_base_url")]
    pub(crate) base_url: String,
    #[serde(default)]
    pub(crate) api_key: Option<String>,
    #[serde(default)]
    pub(crate) api_key_env: Option<String>,
    pub(crate) model: String,
    #[serde(default = "default_true")]
    pub(crate) send_dimensions: bool,
    #[serde(default = "default_encoding_format")]
    pub(crate) encoding_format: EncodingFormat,
    #[serde(default)]
    pub(crate) user: Option<String>,
    #[serde(default)]
    pub(crate) organization: Option<String>,
    #[serde(default)]
    pub(crate) project: Option<String>,
    #[serde(default)]
    pub(crate) headers: BTreeMap<String, String>,
    #[serde(default = "default_timeout_ms")]
    pub(crate) timeout_ms: u64,
    #[serde(default = "default_max_retries")]
    pub(crate) max_retries: u32,
    #[serde(default = "default_batch_size")]
    pub(crate) batch_size: usize,
    #[serde(default)]
    pub(crate) semantic_identity: Option<String>,
}

impl ProviderConfig {
    pub(crate) fn parse(bytes: &[u8]) -> Result<Self, String> {
        let mut config: Self = serde_json::from_slice(bytes)
            .map_err(|error| format!("invalid OpenAI-compatible providerConfig: {error}"))?;
        config.validate()?;
        config.base_url = normalize_base_url(&config.base_url)?;
        Ok(config)
    }

    fn validate(&self) -> Result<(), String> {
        self.validate_strings()?;
        self.validate_execution_limits()?;
        self.validate_semantic_identity()?;
        validate_headers(&self.headers)?;
        let _ = normalize_base_url(&self.base_url)?;
        Ok(())
    }

    fn validate_strings(&self) -> Result<(), String> {
        require_nonempty("model", &self.model)?;
        validate_string("model", &self.model)?;
        validate_string("base_url", &self.base_url)?;
        validate_optional_nonempty("api_key", self.api_key.as_deref())?;
        validate_optional_nonempty("api_key_env", self.api_key_env.as_deref())?;
        validate_optional_string("user", self.user.as_deref())?;
        validate_optional_string("organization", self.organization.as_deref())?;
        validate_optional_string("project", self.project.as_deref())?;
        Ok(())
    }

    fn validate_execution_limits(&self) -> Result<(), String> {
        if !(1..=MAX_TIMEOUT_MS).contains(&self.timeout_ms) {
            return Err(format!("timeout_ms must be between 1 and {MAX_TIMEOUT_MS}"));
        }
        if self.max_retries > MAX_RETRIES {
            return Err(format!("max_retries must be between 0 and {MAX_RETRIES}"));
        }
        if !(1..=MAX_BATCH_SIZE).contains(&self.batch_size) {
            return Err(format!("batch_size must be between 1 and {MAX_BATCH_SIZE}"));
        }
        Ok(())
    }

    fn validate_semantic_identity(&self) -> Result<(), String> {
        if let Some(identity) = self.semantic_identity.as_deref() {
            require_nonempty("semantic_identity", identity)?;
            validate_string("semantic_identity", identity)?;
            if identity.len() > MAX_SEMANTIC_IDENTITY_BYTES {
                return Err(format!(
                    "semantic_identity must be at most {MAX_SEMANTIC_IDENTITY_BYTES} bytes"
                ));
            }
        }
        Ok(())
    }

    pub(crate) fn agent(&self) -> Agent {
        Agent::config_builder()
            .timeout_global(Some(Duration::from_millis(self.timeout_ms)))
            .http_status_as_error(false)
            .proxy(None)
            .max_redirects(0)
            .build()
            .into()
    }

    pub(crate) fn embeddings_url(&self) -> String {
        format!("{}/embeddings", self.base_url)
    }

    pub(crate) fn final_headers(
        &self,
        mut env_lookup: impl FnMut(&str) -> Result<Option<String>, String>,
    ) -> Result<Vec<(String, String)>, String> {
        let mut headers = BTreeMap::<String, (String, String)>::new();
        put_header(&mut headers, "Content-Type", "application/json");
        if let Some(value) = &self.organization {
            put_header(&mut headers, "OpenAI-Organization", value);
        }
        if let Some(value) = &self.project {
            put_header(&mut headers, "OpenAI-Project", value);
        }

        let custom_authorization = self
            .headers
            .keys()
            .any(|name| name.eq_ignore_ascii_case("authorization"));
        if !custom_authorization {
            if let Some(value) = &self.api_key {
                put_header(&mut headers, "Authorization", &format!("Bearer {value}"));
            } else if let Some(name) = &self.api_key_env {
                let value = env_lookup(name)?
                    .filter(|value| !value.is_empty())
                    .ok_or_else(|| format!("api_key_env variable {name:?} is missing or empty"))?;
                if value.contains('\0') {
                    return Err(format!(
                        "api_key_env variable {name:?} contains an invalid NUL byte"
                    ));
                }
                put_header(&mut headers, "Authorization", &format!("Bearer {value}"));
            }
        }
        for (name, value) in &self.headers {
            put_header(&mut headers, name, value);
        }
        let result = headers.into_values().collect::<Vec<_>>();
        validate_final_headers(&result)?;
        let bytes = result
            .iter()
            .map(|(name, value)| name.len() + value.len() + 4)
            .sum::<usize>();
        if bytes > MAX_FINAL_HEADER_BYTES {
            return Err(format!(
                "final request headers exceed the {MAX_FINAL_HEADER_BYTES}-byte limit"
            ));
        }
        Ok(result)
    }
}

fn validate_headers(headers: &BTreeMap<String, String>) -> Result<(), String> {
    let mut names = BTreeSet::new();
    let mut value_bytes = 0_usize;
    for (name, value) in headers {
        let lower = name.to_ascii_lowercase();
        if !names.insert(lower) {
            return Err(format!(
                "headers contains duplicate case-insensitive header name {name:?}"
            ));
        }
        HeaderName::try_from(name.as_str())
            .map_err(|_| format!("headers contains invalid header name {name:?}"))?;
        HeaderValue::try_from(value.as_str())
            .map_err(|_| format!("headers contains invalid value for {name:?}"))?;
        value_bytes = value_bytes
            .checked_add(value.len())
            .ok_or_else(|| "custom header size overflow".to_owned())?;
    }
    if value_bytes > MAX_CUSTOM_HEADER_VALUE_BYTES {
        return Err(format!(
            "custom header values exceed the {MAX_CUSTOM_HEADER_VALUE_BYTES}-byte limit"
        ));
    }
    Ok(())
}

fn validate_final_headers(headers: &[(String, String)]) -> Result<(), String> {
    for (name, value) in headers {
        HeaderName::try_from(name.as_str())
            .map_err(|_| format!("final request contains invalid header name {name:?}"))?;
        HeaderValue::try_from(value.as_str())
            .map_err(|_| format!("final request header {name:?} has an invalid value"))?;
    }
    Ok(())
}

fn put_header(headers: &mut BTreeMap<String, (String, String)>, name: &str, value: &str) {
    headers.insert(
        name.to_ascii_lowercase(),
        (name.to_owned(), value.to_owned()),
    );
}

fn normalize_base_url(value: &str) -> Result<String, String> {
    require_nonempty("base_url", value)?;
    let normalized = value.trim_end_matches('/');
    require_nonempty("base_url", normalized)?;
    let uri = Uri::try_from(normalized)
        .map_err(|_| "base_url must be an absolute http:// or https:// URL".to_owned())?;
    let scheme = uri.scheme_str().unwrap_or_default();
    if !matches!(scheme, "http" | "https") || uri.authority().is_none() {
        return Err("base_url must be an absolute http:// or https:// URL".to_owned());
    }
    if uri.query().is_some() {
        return Err("base_url must not contain a query or fragment".to_owned());
    }
    Ok(normalized.to_owned())
}

fn require_nonempty(name: &str, value: &str) -> Result<(), String> {
    if value.is_empty() {
        Err(format!("{name} must be a non-empty STRING"))
    } else {
        Ok(())
    }
}

fn validate_string(name: &str, value: &str) -> Result<(), String> {
    if value.contains('\0') {
        Err(format!("{name} must not contain NUL"))
    } else {
        Ok(())
    }
}

fn validate_optional_nonempty(name: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        require_nonempty(name, value)?;
        validate_string(name, value)?;
    }
    Ok(())
}

fn validate_optional_string(name: &str, value: Option<&str>) -> Result<(), String> {
    if let Some(value) = value {
        validate_string(name, value)?;
    }
    Ok(())
}

fn default_base_url() -> String {
    DEFAULT_BASE_URL.to_owned()
}

const fn default_true() -> bool {
    true
}

const fn default_encoding_format() -> EncodingFormat {
    EncodingFormat::Float
}

const fn default_timeout_ms() -> u64 {
    DEFAULT_TIMEOUT_MS
}

const fn default_max_retries() -> u32 {
    DEFAULT_MAX_RETRIES
}

const fn default_batch_size() -> usize {
    DEFAULT_BATCH_SIZE
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(value: serde_json::Value) -> Result<ProviderConfig, String> {
        ProviderConfig::parse(
            serde_json::to_vec(&value)
                .expect("serialize provider config")
                .as_slice(),
        )
    }

    #[test]
    fn defaults_and_complete_surface_parse_locally() {
        let defaults = parse(serde_json::json!({"model":"embed"})).expect("defaults");
        assert_eq!(defaults.base_url, DEFAULT_BASE_URL);
        assert!(defaults.send_dimensions);
        assert_eq!(defaults.encoding_format, EncodingFormat::Float);
        assert_eq!(defaults.timeout_ms, DEFAULT_TIMEOUT_MS);
        assert_eq!(defaults.max_retries, DEFAULT_MAX_RETRIES);
        assert_eq!(defaults.batch_size, DEFAULT_BATCH_SIZE);
        let agent = defaults.agent();
        assert!(agent.config().proxy().is_none());
        assert_eq!(agent.config().max_redirects(), 0);

        let complete = parse(serde_json::json!({
            "base_url":"http://localhost:8080/v1/",
            "api_key":"direct",
            "api_key_env":"TEST_KEY",
            "model":"custom",
            "send_dimensions":false,
            "encoding_format":"base64",
            "user":"u",
            "organization":"org",
            "project":"project",
            "headers":{"X-Test":"value"},
            "timeout_ms":1234,
            "max_retries":4,
            "batch_size":16,
            "semantic_identity":"deployment-2"
        }))
        .expect("complete config");
        assert_eq!(complete.base_url, "http://localhost:8080/v1");
        assert_eq!(complete.encoding_format, EncodingFormat::Base64);
        assert_eq!(complete.semantic_identity.as_deref(), Some("deployment-2"));
    }

    #[test]
    fn unknown_types_ranges_and_duplicate_headers_fail() {
        assert!(parse(serde_json::json!({"model":"x","unknown":1})).is_err());
        assert!(parse(serde_json::json!({"model":""})).is_err());
        assert!(parse(serde_json::json!({"model":"x","timeout_ms":0})).is_err());
        assert!(parse(serde_json::json!({"model":"x","max_retries":9})).is_err());
        assert!(parse(serde_json::json!({"model":"x","batch_size":2049})).is_err());
        assert!(
            parse(serde_json::json!({
                "model":"x",
                "headers":{"Authorization":"a","authorization":"b"}
            }))
            .is_err()
        );
        assert!(parse(serde_json::json!({"model":"x","base_url":"file:///tmp"})).is_err());
        assert!(
            parse(serde_json::json!({
                "model":"x",
                "base_url":"https://example.test/v1?tenant=1"
            }))
            .is_err()
        );
    }

    #[test]
    fn authorization_precedence_does_not_read_environment_when_overridden() {
        let custom = parse(serde_json::json!({
            "model":"x",
            "api_key":"direct",
            "api_key_env":"NEVER_READ",
            "headers":{"Authorization":"Custom token"}
        }))
        .expect("custom auth");
        let mut lookups = 0;
        let headers = custom
            .final_headers(|_| {
                lookups += 1;
                Ok(Some("env-secret".to_owned()))
            })
            .expect("headers");
        assert_eq!(lookups, 0);
        assert!(headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Custom token"
        }));

        let direct = parse(serde_json::json!({
            "model":"x",
            "api_key":"direct-secret",
            "api_key_env":"NEVER_READ"
        }))
        .expect("direct auth");
        let headers = direct
            .final_headers(|_| panic!("environment must not be read"))
            .expect("headers");
        assert!(headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer direct-secret"
        }));
    }

    #[test]
    fn explicit_environment_name_is_resolved_per_request() {
        let config = parse(serde_json::json!({
            "model":"x",
            "api_key_env":"LITHOGRAPH_PHASE13_TEST_KEY"
        }))
        .expect("config");
        let headers = config
            .final_headers(|name| {
                assert_eq!(name, "LITHOGRAPH_PHASE13_TEST_KEY");
                Ok(Some("rotated-secret".to_owned()))
            })
            .expect("headers");
        assert!(headers.iter().any(|(name, value)| {
            name.eq_ignore_ascii_case("authorization") && value == "Bearer rotated-secret"
        }));
        assert!(
            config
                .final_headers(|_| Ok(None))
                .expect_err("missing env")
                .contains("LITHOGRAPH_PHASE13_TEST_KEY")
        );
    }

    #[test]
    fn generated_header_validation_rejects_invalid_credentials_without_echoing_them() {
        let direct = parse(serde_json::json!({
            "model":"x",
            "api_key":"secret\r\ninjected"
        }))
        .expect("direct config");
        let error = direct
            .final_headers(|_| panic!("environment must not be read"))
            .expect_err("invalid direct credential");
        assert!(error.contains("Authorization"));
        assert!(!error.contains("secret"));
        assert!(!error.contains("injected"));

        let env = parse(serde_json::json!({
            "model":"x",
            "api_key_env":"LITHOGRAPH_PHASE13_TEST_KEY"
        }))
        .expect("env config");
        let error = env
            .final_headers(|_| Ok(Some("rotated\nsecret".to_owned())))
            .expect_err("invalid environment credential");
        assert!(error.contains("Authorization"));
        assert!(!error.contains("rotated"));
        assert!(!error.contains("secret"));
    }
}
