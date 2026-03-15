use super::*;

use crate::error::RuntimeError;
use async_trait::async_trait;
use serde_json::{Value, json};

pub struct WebSearchTool;

#[async_trait]
impl Tool for WebSearchTool {
    fn definition(&self) -> ToolDefinition {
        ToolDefinition {
            name: "web_search".to_string(),
            description: "Search the web for lightweight results with titles, urls, and snippets."
                .to_string(),
            parameters_schema: json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "minimum": 1 }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        }
    }

    fn validate(&self, params: &Value) -> Result<(), RuntimeError> {
        require_string(params, "web_search", "query")?;
        optional_usize(params, "web_search", "limit")?;
        Ok(())
    }

    async fn call(&self, params: Value, _context: &ToolContext) -> Result<Value, RuntimeError> {
        let query = require_string(&params, "web_search", "query")?;
        let limit =
            optional_usize(&params, "web_search", "limit")?.unwrap_or(DEFAULT_WEB_SEARCH_LIMIT);
        let client = reqwest::Client::new();
        let response = client
            .get("https://html.duckduckgo.com/html/")
            .query(&[("q", query.as_str())])
            .send()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_search".to_string(),
                reason: error.to_string(),
            })?;
        let body = response
            .text()
            .await
            .map_err(|error| RuntimeError::ToolExecution {
                tool: "web_search".to_string(),
                reason: error.to_string(),
            })?;

        let link_regex = regex::Regex::new(
            r#"<a[^>]*class="[^"]*result__a[^"]*"[^>]*href="(?P<url>[^"]+)"[^>]*>(?P<title>.*?)</a>"#,
        )
        .unwrap();
        let snippet_regex = regex::Regex::new(
            r#"<a[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(?P<snippet>.*?)</a>|<div[^>]*class="[^"]*result__snippet[^"]*"[^>]*>(?P<snippet_div>.*?)</div>"#,
        )
        .unwrap();

        let snippets = snippet_regex
            .captures_iter(&body)
            .filter_map(|captures| {
                captures
                    .name("snippet")
                    .or_else(|| captures.name("snippet_div"))
            })
            .map(|capture| {
                normalize_whitespace(&decode_html_entities(&strip_tags(capture.as_str())))
            })
            .collect::<Vec<_>>();

        let mut results = Vec::new();
        for (index, captures) in link_regex.captures_iter(&body).enumerate() {
            if results.len() >= limit {
                break;
            }
            let Some(url) = captures.name("url") else {
                continue;
            };
            let Some(title) = captures.name("title") else {
                continue;
            };
            results.push(json!({
                "title": normalize_whitespace(&decode_html_entities(&strip_tags(title.as_str()))),
                "url": decode_html_entities(url.as_str()),
                "snippet": snippets.get(index).cloned().unwrap_or_default(),
            }));
        }

        Ok(json!({
            "query": query,
            "results": results,
            "result_count": results.len()
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
