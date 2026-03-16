use std::time::Duration;

use anyhow::Context;
use chrono::Utc;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use reqwest::{Client, StatusCode};
use serde_json::Value;

#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    pub base_url: String,
    pub timeout: Duration,
    pub provider_name: String,
    pub bearer_token: Option<String>,
    pub extra_headers: Vec<(String, String)>,
}

impl Default for OpenAiCompatibleConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:1234/v1".to_string(),
            timeout: Duration::from_secs(120),
            provider_name: "openai-compatible".to_string(),
            bearer_token: None,
            extra_headers: Vec::new(),
        }
    }
}

impl OpenAiCompatibleConfig {
    pub fn supports_temperature(&self) -> bool {
        !matches!(self.provider_name.as_str(), "copilot")
    }

    pub fn build_client(&self, accept_sse: bool) -> Result<Client, anyhow::Error> {
        let mut headers = HeaderMap::new();
        headers.insert(USER_AGENT, HeaderValue::from_static("BetterClaw"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        if accept_sse {
            headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        }

        if let Some(token) = &self.bearer_token {
            let bearer = format!("Bearer {token}");
            let value = HeaderValue::from_str(&bearer).context("invalid bearer auth header")?;
            headers.insert(AUTHORIZATION, value);
        }

        for (key, value) in &self.extra_headers {
            let name = HeaderName::from_bytes(key.as_bytes())
                .with_context(|| format!("invalid header name '{key}'"))?;
            let value = HeaderValue::from_str(value)
                .with_context(|| format!("invalid header value for '{key}'"))?;
            headers.insert(name, value);
        }

        Client::builder()
            .default_headers(headers)
            .timeout(self.timeout)
            .build()
            .context("failed to build HTTP client")
    }

    pub fn endpoint(&self, suffix: &str) -> String {
        format!("{}/{}", self.base_url.trim_end_matches('/'), suffix)
    }

    pub fn provider_request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
        headers
            .get("x-request-id")
            .or_else(|| headers.get("x-lmstudio-request-id"))
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string)
    }

    pub fn retry_after(headers: &reqwest::header::HeaderMap) -> Option<Duration> {
        let header_value = headers.get("retry-after")?.to_str().ok()?.trim();
        if let Ok(seconds) = header_value.parse::<u64>() {
            return Some(Duration::from_secs(seconds));
        }

        let retry_at = chrono::DateTime::parse_from_rfc2822(header_value)
            .ok()?
            .with_timezone(&Utc);
        let delay = retry_at.signed_duration_since(Utc::now());
        (delay.num_milliseconds() > 0)
            .then(|| Duration::from_millis(delay.num_milliseconds() as u64))
    }

    pub fn rate_limit_message(status: Option<StatusCode>, body: &Value) -> Option<String> {
        let error = body.get("error").unwrap_or(body);
        let code = error
            .get("code")
            .and_then(Value::as_str)
            .map(|value| value.to_ascii_lowercase());
        let message = error
            .get("message")
            .and_then(Value::as_str)
            .or_else(|| body.get("body").and_then(Value::as_str))
            .map(ToString::to_string);

        let status_is_rate_limit = status == Some(StatusCode::TOO_MANY_REQUESTS);
        let code_is_rate_limit = code
            .as_deref()
            .map(Self::looks_like_rate_limit_text)
            .unwrap_or(false);
        let message_is_rate_limit = message
            .as_deref()
            .map(Self::looks_like_rate_limit_text)
            .unwrap_or(false);

        if !(status_is_rate_limit || code_is_rate_limit || message_is_rate_limit) {
            return None;
        }

        Some(
            message
                .filter(|value| !value.trim().is_empty())
                .unwrap_or_else(|| "rate limit exceeded".to_string()),
        )
    }

    pub fn looks_like_rate_limit_text(text: &str) -> bool {
        let normalized = text.to_ascii_lowercase();
        normalized.contains("rate limit")
            || normalized.contains("rate-limit")
            || normalized.contains("rate_limit")
            || normalized.contains("too many requests")
    }
}

#[cfg(test)]
mod tests {
    use chrono::{Duration as ChronoDuration, Utc};
    use reqwest::StatusCode;
    use reqwest::header::{HeaderMap, HeaderValue};
    use serde_json::json;

    use super::OpenAiCompatibleConfig;

    #[test]
    fn parses_retry_after_seconds() {
        let mut headers = HeaderMap::new();
        headers.insert("retry-after", HeaderValue::from_static("12"));
        assert_eq!(
            OpenAiCompatibleConfig::retry_after(&headers),
            Some(std::time::Duration::from_secs(12))
        );
    }

    #[test]
    fn parses_retry_after_http_date() {
        let retry_at = (Utc::now() + ChronoDuration::seconds(30)).to_rfc2822();
        let mut headers = HeaderMap::new();
        headers.insert(
            "retry-after",
            HeaderValue::from_str(&retry_at).expect("valid header"),
        );
        let delay = OpenAiCompatibleConfig::retry_after(&headers).expect("delay");
        assert!(delay.as_secs() <= 30);
        assert!(delay.as_secs() >= 28);
    }

    #[test]
    fn detects_rate_limit_from_body_code() {
        let message = OpenAiCompatibleConfig::rate_limit_message(
            Some(StatusCode::BAD_REQUEST),
            &json!({ "error": { "code": "rate_limit_exceeded", "message": "slow down" } }),
        );
        assert_eq!(message.as_deref(), Some("slow down"));
    }

    #[test]
    fn copilot_disables_temperature_support() {
        let config = OpenAiCompatibleConfig {
            provider_name: "copilot".to_string(),
            ..OpenAiCompatibleConfig::default()
        };
        assert!(!config.supports_temperature());
    }

    #[test]
    fn default_provider_supports_temperature() {
        assert!(OpenAiCompatibleConfig::default().supports_temperature());
    }
}
