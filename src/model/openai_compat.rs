use std::time::Duration;

use anyhow::Context;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use reqwest::Client;

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
}
