use async_trait::async_trait;
use reqwest::header::{
    ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderName, HeaderValue, USER_AGENT,
};
use rust_decimal::Decimal;
use secrecy::ExposeSecret;
use serde::{Deserialize, Serialize};
use serde_json::Value as JsonValue;

use crate::error::LlmError;
use crate::llm::costs;
use crate::llm::provider::{
    ChatMessage, CompletionRequest, CompletionResponse, FinishReason, LlmProvider, ModelMetadata,
    Role, ToolCall, ToolCompletionRequest, ToolCompletionResponse, ToolDefinition,
};
use crate::llm::rig_adapter::normalize_schema_strict;

const DEFAULT_COPILOT_USER_AGENT: &str = "BetterClaw";
const MAX_LOG_BODY_CHARS: usize = 4_000;

pub struct CopilotProvider {
    client: reqwest::Client,
    base_url: String,
    model_name: String,
    input_cost: Decimal,
    output_cost: Decimal,
}

impl CopilotProvider {
    pub fn new(config: &crate::config::CopilotConfig) -> Result<Self, LlmError> {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {}", config.access_token.expose_secret());
        let auth = HeaderValue::from_str(&bearer).map_err(|e| LlmError::RequestFailed {
            provider: "copilot".to_string(),
            reason: format!("Invalid Authorization header: {e}"),
        })?;
        headers.insert(AUTHORIZATION, auth);
        headers.insert(ACCEPT, HeaderValue::from_static("application/json"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(DEFAULT_COPILOT_USER_AGENT),
        );

        for (key, value) in crate::config::llm::build_copilot_headers(config) {
            let name = HeaderName::from_bytes(key.as_bytes()).map_err(|e| LlmError::RequestFailed {
                provider: "copilot".to_string(),
                reason: format!("Invalid header name {key:?}: {e}"),
            })?;
            let val = HeaderValue::from_str(&value).map_err(|e| LlmError::RequestFailed {
                provider: "copilot".to_string(),
                reason: format!("Invalid header value for {key}: {e}"),
            })?;
            headers.insert(name, val);
        }

        let client = reqwest::Client::builder()
            .default_headers(headers)
            .build()
            .map_err(LlmError::Http)?;

        let (input_cost, output_cost) =
            costs::model_cost(&config.model).unwrap_or_else(costs::default_cost);

        Ok(Self {
            client,
            base_url: config.base_url.trim_end_matches('/').to_string(),
            model_name: config.model.clone(),
            input_cost,
            output_cost,
        })
    }

    fn endpoint_url(&self) -> String {
        format!("{}/chat/completions", self.base_url)
    }

    async fn send_request(&self, body: &CopilotChatRequest) -> Result<CopilotParsedResponse, LlmError> {
        let response = self.client.post(self.endpoint_url()).json(body).send().await?;
        let status = response.status();
        let text = response.text().await.unwrap_or_default();

        if status.as_u16() == 401 || status.as_u16() == 403 {
            tracing::warn!(
                status = %status,
                body = %truncate_for_log(&text),
                "Copilot authentication failed"
            );
            return Err(LlmError::AuthFailed {
                provider: "copilot".to_string(),
            });
        }

        if !status.is_success() {
            let reason = extract_error_message(&text)
                .unwrap_or_else(|| format!("HTTP {}: {}", status, truncate_for_log(&text)));
            tracing::warn!(status = %status, body = %truncate_for_log(&text), "Copilot request failed");
            return Err(LlmError::RequestFailed {
                provider: "copilot".to_string(),
                reason,
            });
        }

        match serde_json::from_str::<CopilotChatResponse>(&text) {
            Ok(parsed) => Ok(parsed.into()),
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    body = %truncate_for_log(&text),
                    "Copilot response did not match expected schema; attempting permissive parse"
                );
                let value: JsonValue = serde_json::from_str(&text).map_err(|json_err| LlmError::InvalidResponse {
                    provider: "copilot".to_string(),
                    reason: format!(
                        "Failed to parse Copilot JSON response: {json_err}; body: {}",
                        truncate_for_log(&text)
                    ),
                })?;
                parse_fallback_response(&value).map_err(|fallback_err| LlmError::InvalidResponse {
                    provider: "copilot".to_string(),
                    reason: format!(
                        "Failed to interpret Copilot response after schema mismatch: {fallback_err}; body: {}",
                        truncate_for_log(&text)
                    ),
                })
            }
        }
    }
}

#[async_trait]
impl LlmProvider for CopilotProvider {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        (self.input_cost, self.output_cost)
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let mut messages = request.messages.clone();
        crate::llm::provider::sanitize_tool_messages(&mut messages);
        let body = CopilotChatRequest::from_completion_request(&self.model_name, request, messages);
        let parsed = self.send_request(&body).await?;

        Ok(CompletionResponse {
            content: parsed.content.unwrap_or_default(),
            input_tokens: parsed.input_tokens,
            output_tokens: parsed.output_tokens,
            finish_reason: parsed.finish_reason,
        })
    }

    async fn complete_with_tools(
        &self,
        request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let known_tool_names = request
            .tools
            .iter()
            .map(|t| t.name.clone())
            .collect::<std::collections::HashSet<_>>();

        let mut messages = request.messages.clone();
        crate::llm::provider::sanitize_tool_messages(&mut messages);
        let body = CopilotChatRequest::from_tool_request(&self.model_name, request, messages);
        let mut parsed = self.send_request(&body).await?;

        for tc in &mut parsed.tool_calls {
            let normalized = normalize_tool_name(&tc.name, &known_tool_names);
            if normalized != tc.name {
                tracing::debug!(original = %tc.name, normalized = %normalized, "Normalized Copilot tool call name");
                tc.name = normalized;
            }
        }

        Ok(ToolCompletionResponse {
            content: parsed.content,
            tool_calls: parsed.tool_calls,
            input_tokens: parsed.input_tokens,
            output_tokens: parsed.output_tokens,
            finish_reason: parsed.finish_reason,
        })
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        Ok(ModelMetadata {
            id: self.model_name.clone(),
            context_length: None,
        })
    }
}

#[derive(Debug, Serialize)]
struct CopilotChatRequest {
    model: String,
    messages: Vec<CopilotMessage>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<CopilotTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_tokens: Option<u32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    temperature: Option<f32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    stop: Option<Vec<String>>,
    stream: bool,
}

impl CopilotChatRequest {
    fn from_completion_request(
        default_model: &str,
        request: CompletionRequest,
        messages: Vec<ChatMessage>,
    ) -> Self {
        Self {
            model: request.model.unwrap_or_else(|| default_model.to_string()),
            messages: messages.iter().map(CopilotMessage::from_chat_message).collect(),
            tools: Vec::new(),
            tool_choice: None,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop: request.stop_sequences,
            stream: false,
        }
    }

    fn from_tool_request(
        default_model: &str,
        request: ToolCompletionRequest,
        messages: Vec<ChatMessage>,
    ) -> Self {
        Self {
            model: request.model.unwrap_or_else(|| default_model.to_string()),
            messages: messages.iter().map(CopilotMessage::from_chat_message).collect(),
            tools: request.tools.iter().map(CopilotTool::from_tool).collect(),
            tool_choice: request.tool_choice,
            max_tokens: request.max_tokens,
            temperature: request.temperature,
            stop: None,
            stream: false,
        }
    }
}

#[derive(Debug, Serialize)]
struct CopilotTool {
    #[serde(rename = "type")]
    kind: &'static str,
    function: CopilotToolFunction,
}

impl CopilotTool {
    fn from_tool(tool: &ToolDefinition) -> Self {
        Self {
            kind: "function",
            function: CopilotToolFunction {
                name: tool.name.clone(),
                description: tool.description.clone(),
                parameters: normalize_schema_strict(&tool.parameters),
            },
        }
    }
}

#[derive(Debug, Serialize)]
struct CopilotToolFunction {
    name: String,
    description: String,
    parameters: JsonValue,
}

#[derive(Debug, Serialize)]
struct CopilotMessage {
    role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<JsonValue>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_calls: Option<Vec<CopilotToolCallOut>>,
}

impl CopilotMessage {
    fn from_chat_message(message: &ChatMessage) -> Self {
        let role = match message.role {
            Role::System => "system",
            Role::User => "user",
            Role::Assistant => "assistant",
            Role::Tool => "tool",
        }
        .to_string();

        let content = match message.role {
            Role::User if !message.images.is_empty() => Some(build_user_content(message)),
            Role::Assistant if message.tool_calls.is_some() && message.content.is_empty() => None,
            _ => Some(JsonValue::String(message.content.clone())),
        };

        let tool_calls = message.tool_calls.as_ref().map(|calls| {
            calls.iter()
                .map(|tc| CopilotToolCallOut {
                    id: tc.id.clone(),
                    kind: "function",
                    function: CopilotToolCallFunctionOut {
                        name: tc.name.clone(),
                        arguments: tc.arguments.to_string(),
                    },
                })
                .collect()
        });

        Self {
            role,
            content,
            tool_call_id: message.tool_call_id.clone(),
            name: message.name.clone(),
            tool_calls,
        }
    }
}

fn build_user_content(message: &ChatMessage) -> JsonValue {
    let mut parts = Vec::new();
    if !message.content.is_empty() {
        parts.push(serde_json::json!({
            "type": "text",
            "text": message.content,
        }));
    }
    for image in &message.images {
        parts.push(serde_json::json!({
            "type": "image_url",
            "image_url": { "url": image }
        }));
    }
    JsonValue::Array(parts)
}

#[derive(Debug, Serialize)]
struct CopilotToolCallOut {
    id: String,
    #[serde(rename = "type")]
    kind: &'static str,
    function: CopilotToolCallFunctionOut,
}

#[derive(Debug, Serialize)]
struct CopilotToolCallFunctionOut {
    name: String,
    arguments: String,
}

#[derive(Debug, Deserialize)]
struct CopilotChatResponse {
    choices: Vec<CopilotChoice>,
    usage: Option<CopilotUsage>,
}

#[derive(Debug, Deserialize)]
struct CopilotChoice {
    finish_reason: Option<String>,
    message: CopilotResponseMessage,
}

#[derive(Debug, Deserialize)]
struct CopilotResponseMessage {
    #[serde(default)]
    content: Option<JsonValue>,
    #[serde(default)]
    tool_calls: Vec<CopilotToolCallIn>,
}

#[derive(Debug, Deserialize)]
struct CopilotToolCallIn {
    id: Option<String>,
    function: CopilotToolCallFunctionIn,
}

#[derive(Debug, Deserialize)]
struct CopilotToolCallFunctionIn {
    name: String,
    arguments: JsonValue,
}

#[derive(Debug, Deserialize)]
struct CopilotUsage {
    #[serde(default)]
    prompt_tokens: u32,
    #[serde(default)]
    completion_tokens: u32,
}

struct CopilotParsedResponse {
    content: Option<String>,
    tool_calls: Vec<ToolCall>,
    input_tokens: u32,
    output_tokens: u32,
    finish_reason: FinishReason,
}

impl From<CopilotChatResponse> for CopilotParsedResponse {
    fn from(value: CopilotChatResponse) -> Self {
        let choice = value.choices.into_iter().next();
        let usage = value.usage.unwrap_or(CopilotUsage {
            prompt_tokens: 0,
            completion_tokens: 0,
        });

        if let Some(choice) = choice {
            let raw_finish_reason = map_finish_reason(choice.finish_reason.as_deref());
            let content = extract_text_content(choice.message.content.as_ref());
            let tool_calls = extract_tool_calls_from_response_message(&choice.message);
            let finish_reason = normalize_finish_reason(raw_finish_reason, &tool_calls, content.as_deref());
            log_empty_tool_use_mismatch(
                raw_finish_reason,
                &tool_calls,
                content.as_deref(),
                choice.message.content.as_ref(),
                false,
            );
            Self {
                content,
                tool_calls,
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                finish_reason,
            }
        } else {
            Self {
                content: None,
                tool_calls: Vec::new(),
                input_tokens: usage.prompt_tokens,
                output_tokens: usage.completion_tokens,
                finish_reason: FinishReason::Unknown,
            }
        }
    }
}

fn parse_fallback_response(value: &JsonValue) -> Result<CopilotParsedResponse, String> {
    let choices = value
        .get("choices")
        .and_then(JsonValue::as_array)
        .ok_or_else(|| "missing choices array".to_string())?;
    let choice = choices
        .first()
        .ok_or_else(|| "response contained no choices".to_string())?;
    let message = choice
        .get("message")
        .ok_or_else(|| "choice missing message object".to_string())?;

    let raw_finish_reason = map_finish_reason(choice.get("finish_reason").and_then(JsonValue::as_str));
    let content = extract_text_content(message.get("content"));
    let tool_calls = extract_tool_calls_from_message_json(message);
    let finish_reason = normalize_finish_reason(raw_finish_reason, &tool_calls, content.as_deref());
    log_empty_tool_use_mismatch(
        raw_finish_reason,
        &tool_calls,
        content.as_deref(),
        Some(message),
        true,
    );

    let input_tokens = value
        .get("usage")
        .and_then(|usage| usage.get("prompt_tokens"))
        .and_then(JsonValue::as_u64)
        .unwrap_or_default() as u32;
    let output_tokens = value
        .get("usage")
        .and_then(|usage| usage.get("completion_tokens"))
        .and_then(JsonValue::as_u64)
        .unwrap_or_default() as u32;

    Ok(CopilotParsedResponse {
        content,
        tool_calls,
        input_tokens,
        output_tokens,
        finish_reason,
    })
}

fn extract_text_content(content: Option<&JsonValue>) -> Option<String> {
    match content {
        Some(JsonValue::String(text)) => Some(text.clone()),
        Some(JsonValue::Array(parts)) => {
            let text = parts
                .iter()
                .filter_map(|part| {
                    part.get("text")
                        .and_then(JsonValue::as_str)
                        .map(ToOwned::to_owned)
                        .or_else(|| part.get("content").and_then(JsonValue::as_str).map(ToOwned::to_owned))
                })
                .collect::<Vec<_>>()
                .join("");
            if text.is_empty() {
                None
            } else {
                Some(text)
            }
        }
        Some(JsonValue::Object(obj)) => obj
            .get("text")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned),
        _ => None,
    }
}

fn parse_tool_arguments(arguments: JsonValue) -> JsonValue {
    match arguments {
        JsonValue::String(s) => serde_json::from_str(&s).unwrap_or(JsonValue::String(s)),
        other => other,
    }
}

fn map_finish_reason(reason: Option<&str>) -> FinishReason {
    match reason {
        Some("stop") => FinishReason::Stop,
        Some("length") => FinishReason::Length,
        Some("tool_calls") | Some("tool_use") => FinishReason::ToolUse,
        Some("content_filter") => FinishReason::ContentFilter,
        _ => FinishReason::Unknown,
    }
}

fn normalize_finish_reason(
    finish_reason: FinishReason,
    tool_calls: &[ToolCall],
    content: Option<&str>,
) -> FinishReason {
    if finish_reason == FinishReason::ToolUse && tool_calls.is_empty() {
        if content.is_some_and(|text| !text.trim().is_empty()) {
            FinishReason::Stop
        } else {
            FinishReason::Unknown
        }
    } else {
        finish_reason
    }
}

fn log_empty_tool_use_mismatch(
    finish_reason: FinishReason,
    tool_calls: &[ToolCall],
    content: Option<&str>,
    raw_message: Option<&JsonValue>,
    fallback: bool,
) {
    if finish_reason != FinishReason::ToolUse || !tool_calls.is_empty() {
        return;
    }

    let parser = if fallback { "fallback" } else { "primary" };
    if content.is_some_and(|text| !text.trim().is_empty()) {
        tracing::debug!(
            parser,
            message = %truncate_json_for_log(raw_message),
            "Copilot reported tool use without structured calls; treating response as text"
        );
    } else {
        tracing::warn!(
            parser,
            message = %truncate_json_for_log(raw_message),
            "Copilot returned tool-use finish reason but no tool calls were extracted"
        );
    }
}

fn extract_tool_calls_from_response_message(message: &CopilotResponseMessage) -> Vec<ToolCall> {
    let mut tool_calls: Vec<ToolCall> = message
        .tool_calls
        .iter()
        .enumerate()
        .map(|(idx, tc)| ToolCall {
            id: tc
                .id
                .clone()
                .unwrap_or_else(|| format!("copilot_tool_call_{idx}")),
            name: tc.function.name.clone(),
            arguments: parse_tool_arguments(tc.function.arguments.clone()),
        })
        .collect();

    if tool_calls.is_empty() {
        tool_calls.extend(extract_tool_calls_from_content(message.content.as_ref()));
    }

    tool_calls
}

fn extract_tool_calls_from_message_json(message: &JsonValue) -> Vec<ToolCall> {
    let mut tool_calls = message
        .get("tool_calls")
        .and_then(JsonValue::as_array)
        .map(|calls| {
            calls.iter()
                .enumerate()
                .filter_map(tool_call_from_openai_json)
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();

    if tool_calls.is_empty() {
        tool_calls.extend(extract_tool_calls_from_content(message.get("content")));
    }

    tool_calls
}

fn tool_call_from_openai_json((idx, tc): (usize, &JsonValue)) -> Option<ToolCall> {
    let function = tc.get("function")?;
    let name = function.get("name")?.as_str()?.to_string();
    let arguments = function.get("arguments").cloned().unwrap_or(JsonValue::Null);
    Some(ToolCall {
        id: tc
            .get("id")
            .and_then(JsonValue::as_str)
            .map(ToOwned::to_owned)
            .unwrap_or_else(|| format!("copilot_tool_call_{idx}")),
        name,
        arguments: parse_tool_arguments(arguments),
    })
}

fn extract_tool_calls_from_content(content: Option<&JsonValue>) -> Vec<ToolCall> {
    match content {
        Some(JsonValue::Array(parts)) => parts
            .iter()
            .enumerate()
            .filter_map(tool_call_from_content_part)
            .collect(),
        Some(value @ JsonValue::Object(_)) => tool_call_from_content_part((0, value)).into_iter().collect(),
        _ => Vec::new(),
    }
}

fn tool_call_from_content_part((idx, part): (usize, &JsonValue)) -> Option<ToolCall> {
    let kind = part.get("type").and_then(JsonValue::as_str).unwrap_or_default();
    if !matches!(kind, "tool_use" | "function_call" | "tool_call") {
        return None;
    }

    let name = part
        .get("name")
        .and_then(JsonValue::as_str)
        .or_else(|| {
            part.get("function")
                .and_then(|function| function.get("name"))
                .and_then(JsonValue::as_str)
        })?
        .to_string();

    let arguments = part
        .get("input")
        .cloned()
        .or_else(|| part.get("arguments").cloned())
        .or_else(|| {
            part.get("function")
                .and_then(|function| function.get("arguments"))
                .cloned()
        })
        .unwrap_or(JsonValue::Object(serde_json::Map::new()));

    let id = part
        .get("id")
        .and_then(JsonValue::as_str)
        .or_else(|| part.get("tool_use_id").and_then(JsonValue::as_str))
        .map(ToOwned::to_owned)
        .unwrap_or_else(|| format!("copilot_tool_call_{idx}"));

    Some(ToolCall {
        id,
        name,
        arguments: parse_tool_arguments(arguments),
    })
}

fn extract_error_message(body: &str) -> Option<String> {
    let value: JsonValue = serde_json::from_str(body).ok()?;
    value.get("message")
        .and_then(JsonValue::as_str)
        .map(ToOwned::to_owned)
        .or_else(|| {
            value
                .get("error")
                .and_then(|err| err.get("message"))
                .and_then(JsonValue::as_str)
                .map(ToOwned::to_owned)
        })
}

fn truncate_for_log(body: &str) -> String {
    let mut chars = body.chars();
    let truncated: String = chars.by_ref().take(MAX_LOG_BODY_CHARS).collect();
    if chars.next().is_some() {
        format!("{truncated}...[truncated]")
    } else {
        truncated
    }
}

fn truncate_json_for_log(value: Option<&JsonValue>) -> String {
    value
        .map(|json| truncate_for_log(&json.to_string()))
        .unwrap_or_default()
}

fn normalize_tool_name(name: &str, known_tools: &std::collections::HashSet<String>) -> String {
    if known_tools.contains(name) {
        return name.to_string();
    }

    if let Some(stripped) = name.strip_prefix("proxy_")
        && known_tools.contains(stripped)
    {
        return stripped.to_string();
    }

    name.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_simple_completion_response() {
        let body = serde_json::json!({
            "choices": [{
                "finish_reason": "stop",
                "message": { "role": "assistant", "content": "ok" }
            }],
            "usage": { "prompt_tokens": 14, "completion_tokens": 4 }
        });

        let parsed = parse_fallback_response(&body).expect("fallback parse should succeed");
        assert_eq!(parsed.content.as_deref(), Some("ok"));
        assert!(parsed.tool_calls.is_empty());
        assert_eq!(parsed.input_tokens, 14);
        assert_eq!(parsed.output_tokens, 4);
        assert_eq!(parsed.finish_reason, FinishReason::Stop);
    }

    #[test]
    fn parses_tool_call_response() {
        let body = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_calls",
                "message": {
                    "role": "assistant",
                    "tool_calls": [{
                        "id": "tooluse_123",
                        "type": "function",
                        "function": {
                            "name": "compressor_delta_v0",
                            "arguments": "{\"wake_pack\":{},\"actions\":[]}"
                        }
                    }]
                }
            }],
            "usage": { "prompt_tokens": 580, "completion_tokens": 71 }
        });

        let parsed = parse_fallback_response(&body).expect("fallback parse should succeed");
        assert!(parsed.content.is_none());
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "compressor_delta_v0");
        assert_eq!(parsed.finish_reason, FinishReason::ToolUse);
    }

    #[test]
    fn parses_tool_use_content_blocks() {
        let body = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_use",
                "message": {
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "toolu_01abc",
                        "name": "memory_search",
                        "input": {"query": "recent notes"}
                    }]
                }
            }],
            "usage": { "prompt_tokens": 120, "completion_tokens": 19 }
        });

        let parsed = parse_fallback_response(&body).expect("fallback parse should succeed");
        assert!(parsed.content.is_none());
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].id, "toolu_01abc");
        assert_eq!(parsed.tool_calls[0].name, "memory_search");
        assert_eq!(parsed.tool_calls[0].arguments, serde_json::json!({"query": "recent notes"}));
        assert_eq!(parsed.finish_reason, FinishReason::ToolUse);
    }

    #[test]
    fn downgrades_empty_tool_use_without_calls_to_text_finish() {
        let body = serde_json::json!({
            "choices": [{
                "finish_reason": "tool_use",
                "message": {
                    "role": "assistant",
                    "content": "<think>Need a tool but did not emit one.</think>"
                }
            }],
            "usage": { "prompt_tokens": 90, "completion_tokens": 12 }
        });

        let parsed = parse_fallback_response(&body).expect("fallback parse should succeed");
        assert!(parsed.tool_calls.is_empty());
        assert_eq!(parsed.finish_reason, FinishReason::Stop);
    }
}