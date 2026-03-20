use super::*;

use crate::error::RuntimeError;
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{Value, json};

const BRAVE_SEARCH_ENDPOINT: &str = "https://api.search.brave.com/res/v1/web/search";
const BRAVE_SEARCH_TIMEOUT_SECS: u64 = 15;
const BRAVE_SEARCH_MAX_LIMIT: usize = 10;
const BRAVE_API_KEY_ENV_VAR: &str = "BRAVE_API_KEY";
const BRAVE_COUNTRY_ALL: &str = "ALL";

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web with Brave Search and return structured results."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1, "maximum": BRAVE_SEARCH_MAX_LIMIT },
                    "count": { "type": "integer", "minimum": 1, "maximum": BRAVE_SEARCH_MAX_LIMIT },
                    "country": { "type": "string" },
                    "language": { "type": "string" },
                    "search_lang": { "type": "string" },
                    "ui_lang": { "type": "string" },
                    "freshness": { "type": "string" }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        let query = require_string(params, "web_search", "query")?;
        validate_query(&query)?;
        if let Some(limit) = optional_usize(params, "web_search", "limit")? {
            validate_result_count(limit)?;
        }
        if let Some(count) = optional_usize(params, "web_search", "count")? {
            validate_result_count(count)?;
        }
        let language = optional_string(params, "web_search", "language")?;
        let search_lang = optional_string(params, "web_search", "search_lang")?;
        if language.is_some() && search_lang.is_some() {
            return Err(invalid_tool_parameters(
                "web_search",
                "provide either 'language' or 'search_lang', not both",
            ));
        }

        if let Some(country) = optional_string(params, "web_search", "country")? {
            normalize_country(&country).ok_or_else(|| {
                invalid_tool_parameters(
                    "web_search",
                    format!(
                        "invalid 'country': expected 2-letter code like 'US' or '{}'",
                        BRAVE_COUNTRY_ALL
                    ),
                )
            })?;
        }

        if let Some(language) = language {
            normalize_search_lang(&language).ok_or_else(|| {
                invalid_tool_parameters(
                    "web_search",
                    format!("invalid 'language': expected a supported code like 'en' or 'de'"),
                )
            })?;
        }

        if let Some(search_lang) = search_lang {
            normalize_search_lang(&search_lang).ok_or_else(|| {
                invalid_tool_parameters(
                    "web_search",
                    "invalid 'search_lang': expected a supported Brave language code".to_string(),
                )
            })?;
        }

        if let Some(ui_lang) = optional_string(params, "web_search", "ui_lang")? {
            normalize_ui_lang(&ui_lang).ok_or_else(|| {
                invalid_tool_parameters(
                    "web_search",
                    "invalid 'ui_lang': expected a locale like 'en-US'".to_string(),
                )
            })?;
        }

        if let Some(freshness) = optional_string(params, "web_search", "freshness")? {
            normalize_freshness(&freshness).ok_or_else(|| {
                invalid_tool_parameters(
                    "web_search",
                    "invalid 'freshness': expected 'day', 'week', 'month', 'year', Brave shortcuts, or a date range 'YYYY-MM-DDtoYYYY-MM-DD'".to_string(),
                )
            })?;
        }

        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let request = build_brave_request(&params)?;
        let api_key = std::env::var(BRAVE_API_KEY_ENV_VAR).map_err(|_| RuntimeError::ToolExecution {
            tool: "web_search".to_string(),
            reason: format!(
                "Brave Search API key not configured. Set {}.",
                BRAVE_API_KEY_ENV_VAR
            ),
        })?;
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(BRAVE_SEARCH_TIMEOUT_SECS))
            .build()
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_search".to_string(),
                reason: format!("failed to build Brave Search client: {error}"),
            })?;

        let mut query = vec![
            ("q".to_string(), request.query.clone()),
            ("count".to_string(), request.count.to_string()),
        ];
        if let Some(country) = request.country.as_deref() {
            query.push(("country".to_string(), country.to_string()));
        }
        if let Some(search_lang) = request.search_lang.as_deref() {
            query.push(("search_lang".to_string(), search_lang.to_string()));
        }
        if let Some(ui_lang) = request.ui_lang.as_deref() {
            query.push(("ui_lang".to_string(), ui_lang.to_string()));
        }
        if let Some(freshness) = request.freshness.as_deref() {
            query.push(("freshness".to_string(), freshness.to_string()));
        }

        let response = client
            .get(brave_search_endpoint())
            .header("Accept", "application/json")
            .header("User-Agent", "BetterClaw-WebSearch/0.1")
            .header("X-Subscription-Token", api_key)
            .query(&query)
            .send()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_search".to_string(),
                reason: format!("Brave Search request failed: {error}"),
            })?;

        let status = response.status();
        let response_text = response.text().await.map_err(|error| RuntimeError::ToolExecution {
            tool: "web_search".to_string(),
            reason: format!("failed to read Brave Search response: {error}"),
        })?;

        if !status.is_success() {
            let detail = summarize_http_error_body(&response_text);
            let reason = match status.as_u16() {
                401 | 403 => format!("Brave Search authentication failed (HTTP {}): {}", status.as_u16(), detail),
                429 => format!("Brave Search rate limit reached (HTTP 429): {}", detail),
                code if code >= 500 => format!("Brave Search server error (HTTP {}): {}", code, detail),
                code => format!("Brave Search returned HTTP {}: {}", code, detail),
            };
            return Err(RuntimeError::ToolExecution {
                tool: "web_search".to_string(),
                reason,
            });
        }

        let parsed: BraveSearchResponse =
            serde_json::from_str(&response_text).map_err(|error| RuntimeError::ToolExecution {
                tool: "web_search".to_string(),
                reason: format!("failed to parse Brave Search response: {error}"),
            })?;

        let results = parsed
            .web
            .and_then(|web| web.results)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|result| {
                let title = normalize_whitespace(result.title?.trim());
                let url = result.url?;
                let snippet = normalize_whitespace(result.description.unwrap_or_default().trim());
                let site_name = result
                    .meta_url
                    .as_ref()
                    .and_then(|meta| meta.hostname.as_ref())
                    .map(|value| normalize_whitespace(value.trim()))
                    .filter(|value| !value.is_empty());
                let age = result
                    .age
                    .as_deref()
                    .map(str::trim)
                    .filter(|value| !value.is_empty())
                    .map(ToString::to_string);

                let mut item = json!({
                    "title": title,
                    "url": url,
                    "description": snippet,
                });
                if let Some(site_name) = site_name {
                    item["site_name"] = json!(site_name);
                }
                if let Some(age) = age {
                    item["published"] = json!(age);
                }
                Some(item)
            })
            .collect::<Vec<_>>();

        Ok(json!({
            "query": request.query,
            "result_count": results.len(),
            "results": results,
            "provider": "brave"
        }))
    }
}

pub struct WebFetchTool;

#[async_trait]
impl Tool for WebFetchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_fetch".to_string(),
            description: "Fetch a URL and return normalized text content plus metadata."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "url": { "type": "string" }
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "web_fetch", "url")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let url = require_string(&params, "web_fetch", "url")?;
        let response = reqwest::Client::new()
            .get(&url)
            .send()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_fetch".to_string(),
                reason: error.to_string(),
            })?;
        let status = response.status().as_u16();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response
            .text()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_fetch".to_string(),
                reason: error.to_string(),
            })?;
        let title = regex::Regex::new(r"(?is)<title>(?P<title>.*?)</title>")
            .unwrap()
            .captures(&body)
            .and_then(|captures| captures.name("title"))
            .map(|title| normalize_whitespace(&decode_html_entities(&strip_tags(title.as_str()))));
        let normalized = normalize_whitespace(&decode_html_entities(&strip_tags(&body)));
        let (content, truncated) = truncate_text_by_bytes(&normalized, DEFAULT_MAX_BYTES);

        Ok(json!({
            "url": url,
            "status": status,
            "content_type": content_type,
            "title": title,
            "content": content,
            "truncated": truncated
        }))
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct BraveSearchRequest {
    query: String,
    count: usize,
    country: Option<String>,
    search_lang: Option<String>,
    ui_lang: Option<String>,
    freshness: Option<String>,
}

#[derive(Debug, Deserialize)]
struct BraveSearchResponse {
    web: Option<BraveWebResults>,
}

#[derive(Debug, Deserialize)]
struct BraveWebResults {
    results: Option<Vec<BraveSearchResult>>,
}

#[derive(Debug, Deserialize)]
struct BraveSearchResult {
    title: Option<String>,
    url: Option<String>,
    description: Option<String>,
    age: Option<String>,
    #[serde(default)]
    meta_url: Option<BraveMetaUrl>,
}

#[derive(Debug, Deserialize)]
struct BraveMetaUrl {
    hostname: Option<String>,
}

fn build_brave_request(params: &Value) -> Result<BraveSearchRequest, RuntimeError> {
    let query = require_string(params, "web_search", "query")?;
    validate_query(&query)?;

    let count = optional_usize(params, "web_search", "count")?
        .or(optional_usize(params, "web_search", "limit")?)
        .unwrap_or(DEFAULT_WEB_SEARCH_LIMIT);
    validate_result_count(count)?;

    let language = optional_string(params, "web_search", "language")?;
    let search_lang = optional_string(params, "web_search", "search_lang")?;
    let normalized_search_lang = match (
        language.as_deref().and_then(normalize_search_lang),
        search_lang.as_deref().and_then(normalize_search_lang),
    ) {
        (Some(language), Some(search_lang)) => {
            if language == search_lang {
                Some(search_lang)
            } else {
                Some(search_lang)
            }
        }
        (Some(language), None) => Some(language),
        (None, Some(search_lang)) => Some(search_lang),
        (None, None) => None,
    };
    if language.is_some() && normalized_search_lang.is_none() {
        return Err(invalid_tool_parameters(
            "web_search",
            format!("invalid 'language': expected a supported code like 'en' or 'de'"),
        ));
    }
    if search_lang.is_some() && normalized_search_lang.is_none() {
        return Err(invalid_tool_parameters(
            "web_search",
            "invalid 'search_lang': expected a supported Brave language code".to_string(),
        ));
    }

    let country = optional_string(params, "web_search", "country")?
        .map(|value| {
            normalize_country(&value).ok_or_else(|| {
                invalid_tool_parameters(
                    "web_search",
                    format!(
                        "invalid 'country': expected 2-letter code like 'US' or '{}'",
                        BRAVE_COUNTRY_ALL
                    ),
                )
            })
        })
        .transpose()?;
    let ui_lang = optional_string(params, "web_search", "ui_lang")?
        .map(|value| {
            normalize_ui_lang(&value).ok_or_else(|| {
                invalid_tool_parameters(
                    "web_search",
                    "invalid 'ui_lang': expected a locale like 'en-US'".to_string(),
                )
            })
        })
        .transpose()?;
    let freshness = optional_string(params, "web_search", "freshness")?
        .map(|value| {
            normalize_freshness(&value).ok_or_else(|| {
                invalid_tool_parameters(
                    "web_search",
                    "invalid 'freshness': expected 'day', 'week', 'month', 'year', Brave shortcuts, or a date range 'YYYY-MM-DDtoYYYY-MM-DD'".to_string(),
                )
            })
        })
        .transpose()?;

    Ok(BraveSearchRequest {
        query: query.trim().to_string(),
        count,
        country,
        search_lang: normalized_search_lang,
        ui_lang,
        freshness,
    })
}

fn validate_query(query: &str) -> Result<(), RuntimeError> {
    let trimmed = query.trim();
    if trimmed.is_empty() {
        return Err(invalid_tool_parameters(
            "web_search",
            "'query' must not be empty",
        ));
    }
    if trimmed.len() > 2_000 {
        return Err(invalid_tool_parameters(
            "web_search",
            "'query' exceeds the maximum length of 2000 characters",
        ));
    }
    Ok(())
}

fn validate_result_count(count: usize) -> Result<(), RuntimeError> {
    if count == 0 || count > BRAVE_SEARCH_MAX_LIMIT {
        return Err(invalid_tool_parameters(
            "web_search",
            format!(
                "'count'/'limit' must be between 1 and {}",
                BRAVE_SEARCH_MAX_LIMIT
            ),
        ));
    }
    Ok(())
}

fn normalize_country(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed.eq_ignore_ascii_case(BRAVE_COUNTRY_ALL) {
        return Some(BRAVE_COUNTRY_ALL.to_string());
    }
    if trimmed.len() == 2 && trimmed.bytes().all(|byte| byte.is_ascii_alphabetic()) {
        return Some(trimmed.to_ascii_uppercase());
    }
    None
}

fn normalize_search_lang(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase();
    let mapped = match normalized.as_str() {
        "ja" => "jp",
        "zh" | "zh-cn" | "zh-sg" => "zh-hans",
        "zh-hk" | "zh-tw" => "zh-hant",
        other => other,
    };
    if is_supported_search_lang(mapped) {
        Some(mapped.to_string())
    } else {
        None
    }
}

fn is_supported_search_lang(value: &str) -> bool {
    matches!(
        value,
        "ar"
            | "eu"
            | "bn"
            | "bg"
            | "ca"
            | "zh-hans"
            | "zh-hant"
            | "hr"
            | "cs"
            | "da"
            | "nl"
            | "en"
            | "en-gb"
            | "et"
            | "fi"
            | "fr"
            | "gl"
            | "de"
            | "el"
            | "gu"
            | "he"
            | "hi"
            | "hu"
            | "is"
            | "it"
            | "jp"
            | "kn"
            | "ko"
            | "lv"
            | "lt"
            | "ms"
            | "ml"
            | "mr"
            | "nb"
            | "pl"
            | "pt-br"
            | "pt-pt"
            | "pa"
            | "ro"
            | "ru"
            | "sr"
            | "sk"
            | "sl"
            | "es"
            | "sv"
            | "ta"
            | "te"
            | "th"
            | "tr"
            | "uk"
            | "vi"
    )
}

fn normalize_ui_lang(value: &str) -> Option<String> {
    let trimmed = value.trim();
    let (language, region) = trimmed.split_once('-')?;
    if language.len() != 2
        || region.len() != 2
        || !language.bytes().all(|byte| byte.is_ascii_alphabetic())
        || !region.bytes().all(|byte| byte.is_ascii_alphabetic())
    {
        return None;
    }
    Some(format!(
        "{}-{}",
        language.to_ascii_lowercase(),
        region.to_ascii_uppercase()
    ))
}

fn normalize_freshness(value: &str) -> Option<String> {
    let normalized = value.trim().to_ascii_lowercase();
    match normalized.as_str() {
        "day" | "pd" => Some("pd".to_string()),
        "week" | "pw" => Some("pw".to_string()),
        "month" | "pm" => Some("pm".to_string()),
        "year" | "py" => Some("py".to_string()),
        _ if is_valid_date_range(&normalized) => Some(normalized),
        _ => None,
    }
}

fn is_valid_date_range(value: &str) -> bool {
    let (start, end) = match value.split_once("to") {
        Some(parts) => parts,
        None => return false,
    };
    matches!(normalize_iso_date(start), Some(ref normalized) if normalized == start)
        && matches!(normalize_iso_date(end), Some(ref normalized) if normalized == end)
        && start <= end
}

fn normalize_iso_date(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    if bytes.len() != 10
        || bytes[4] != b'-'
        || bytes[7] != b'-'
        || !bytes
            .iter()
            .enumerate()
            .all(|(index, byte)| index == 4 || index == 7 || byte.is_ascii_digit())
    {
        return None;
    }

    let year = value[0..4].parse::<i32>().ok()?;
    let month = value[5..7].parse::<u32>().ok()?;
    let day = value[8..10].parse::<u32>().ok()?;
    chrono::NaiveDate::from_ymd_opt(year, month, day)?;
    Some(value.to_string())
}

fn summarize_http_error_body(body: &str) -> String {
    let parsed = serde_json::from_str::<Value>(body).ok();
    let detail = parsed
        .as_ref()
        .and_then(|value| {
            value
                .get("message")
                .and_then(Value::as_str)
                .or_else(|| value.get("error").and_then(Value::as_str))
                .or_else(|| {
                    value
                        .get("error")
                        .and_then(Value::as_object)
                        .and_then(|error| error.get("message"))
                        .and_then(Value::as_str)
                })
        })
        .unwrap_or(body);
    let normalized = normalize_whitespace(detail);
    let (truncated, was_truncated) = truncate_text_by_bytes(&normalized, 240);
    if was_truncated {
        format!("{truncated}...")
    } else if truncated.is_empty() {
        "no error body returned".to_string()
    } else {
        truncated
    }
}

fn brave_search_endpoint() -> String {
    std::env::var("BETTERCLAW_BRAVE_SEARCH_BASE_URL")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .unwrap_or_else(|| BRAVE_SEARCH_ENDPOINT.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::{Json, Router, routing::get};
    use std::net::SocketAddr;
    use std::sync::{Arc, Mutex};

    async fn empty_context() -> ToolContext {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.keep();
        let db = Arc::new(crate::db::Db::open(&root.join("test.db")).await.unwrap());
        ToolContext::new(
            crate::workspace::Workspace::new("default", root),
            "thread-id",
            "external-thread-id",
            "web",
            db,
        )
    }

    #[test]
    fn build_request_normalizes_model_friendly_inputs() {
        let request = build_brave_request(&json!({
            "query": "rust async",
            "limit": 3,
            "country": "us",
            "language": "ja",
            "ui_lang": "en-us",
            "freshness": "week"
        }))
        .unwrap();

        assert_eq!(
            request,
            BraveSearchRequest {
                query: "rust async".to_string(),
                count: 3,
                country: Some("US".to_string()),
                search_lang: Some("jp".to_string()),
                ui_lang: Some("en-US".to_string()),
                freshness: Some("pw".to_string()),
            }
        );
    }

    #[test]
    fn build_request_accepts_both_count_and_limit() {
        let request = build_brave_request(&json!({
            "query": "rust async",
            "limit": 3,
            "count": 5
        }))
        .unwrap();

        assert_eq!(request.count, 5);
    }

    #[test]
    fn build_request_rejects_invalid_date_range() {
        let error = build_brave_request(&json!({
            "query": "rust",
            "freshness": "2024-02-30to2024-03-01"
        }))
        .unwrap_err();

        match error {
            RuntimeError::InvalidToolParameters { tool, reason } => {
                assert_eq!(tool, "web_search");
                assert!(reason.contains("invalid 'freshness'"));
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn web_search_calls_brave_api_and_shapes_results() {
        let captured = Arc::new(Mutex::new(None::<(String, String)>));
        let captured_clone = Arc::clone(&captured);
        let app = Router::new().route(
            "/res/v1/web/search",
            get(move |request: axum::extract::Request| {
                let captured = Arc::clone(&captured_clone);
                async move {
                    let query = request.uri().query().unwrap_or_default().to_string();
                    let token = request
                        .headers()
                        .get("x-subscription-token")
                        .and_then(|value| value.to_str().ok())
                        .unwrap_or_default()
                        .to_string();
                    *captured.lock().unwrap() = Some((query, token));
                    Json(json!({
                        "web": {
                            "results": [
                                {
                                    "title": "Rust Async Patterns",
                                    "url": "https://example.com/rust-async",
                                    "description": "  A guide to async Rust.  ",
                                    "age": "2 days ago",
                                    "meta_url": { "hostname": "example.com" }
                                }
                            ]
                        }
                    }))
                }
            }),
        );
        let address = spawn_test_server(app).await;
        unsafe {
            std::env::set_var("BRAVE_API_KEY", "test-brave-key");
            std::env::set_var(
                "BETTERCLAW_BRAVE_SEARCH_BASE_URL",
                format!("http://{address}/res/v1/web/search"),
            );
        }

        let result = WebSearchTool
            .call(
                json!({
                    "query": "rust async",
                    "count": 1,
                    "country": "us",
                    "language": "zh",
                    "ui_lang": "en-us",
                    "freshness": "day"
                }),
                &empty_context().await,
            )
            .await
            .unwrap();

        let captured = captured.lock().unwrap().clone().unwrap();
        assert!(captured.0.contains("q=rust+async") || captured.0.contains("q=rust%20async"));
        assert!(captured.0.contains("count=1"));
        assert!(captured.0.contains("country=US"));
        assert!(captured.0.contains("search_lang=zh-hans"));
        assert!(captured.0.contains("ui_lang=en-US"));
        assert!(captured.0.contains("freshness=pd"));
        assert_eq!(captured.1, "test-brave-key");

        assert_eq!(result["provider"], "brave");
        assert_eq!(result["result_count"], 1);
        assert_eq!(result["results"][0]["title"], "Rust Async Patterns");
        assert_eq!(result["results"][0]["url"], "https://example.com/rust-async");
        assert_eq!(result["results"][0]["description"], "A guide to async Rust.");
        assert_eq!(result["results"][0]["site_name"], "example.com");
        assert_eq!(result["results"][0]["published"], "2 days ago");

        unsafe {
            std::env::remove_var("BRAVE_API_KEY");
            std::env::remove_var("BETTERCLAW_BRAVE_SEARCH_BASE_URL");
        }
    }

    #[tokio::test]
    async fn web_search_surfaces_auth_and_http_failures() {
        let app = Router::new().route(
            "/res/v1/web/search",
            get(|| async { (axum::http::StatusCode::UNAUTHORIZED, Json(json!({"message": "bad key"}))) }),
        );
        let address = spawn_test_server(app).await;
        unsafe {
            std::env::set_var("BRAVE_API_KEY", "bad-key");
            std::env::set_var(
                "BETTERCLAW_BRAVE_SEARCH_BASE_URL",
                format!("http://{address}/res/v1/web/search"),
            );
        }

        let error = WebSearchTool
            .call(json!({ "query": "rust" }), &empty_context().await)
            .await
            .unwrap_err();

        match error {
            RuntimeError::ToolExecution { tool, reason } => {
                assert_eq!(tool, "web_search");
                assert!(reason.contains("authentication failed"));
                assert!(reason.contains("bad key"));
            }
            other => panic!("unexpected error: {other:?}"),
        }

        unsafe {
            std::env::remove_var("BRAVE_API_KEY");
            std::env::remove_var("BETTERCLAW_BRAVE_SEARCH_BASE_URL");
        }
    }

    async fn spawn_test_server(app: Router) -> SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        address
    }
}
