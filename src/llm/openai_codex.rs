use async_trait::async_trait;
use futures::StreamExt;
use reqwest::header::{ACCEPT, AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use rust_decimal::Decimal;
use serde::Serialize;
use serde_json::Value as JsonValue;

use crate::error::LlmError;
use crate::llm::costs;
use crate::llm::provider::{
    ChatMessage, CompletionRequest, CompletionResponse, FinishReason, LlmProvider, ModelMetadata,
    Role, ToolCall, ToolCompletionRequest, ToolCompletionResponse, ToolDefinition,
};
use crate::llm::rig_adapter::normalize_schema_strict;

const DEFAULT_CODEX_USER_AGENT: &str = "BetterClaw";

pub struct OpenAiCodexProvider {
    client: reqwest::Client,
    base_url: String,
    model_name: String,
    input_cost: Decimal,
    output_cost: Decimal,
}

impl OpenAiCodexProvider {
    pub fn new(config: &crate::config::OpenAiCodexConfig) -> Result<Self, LlmError> {
        let mut headers = HeaderMap::new();
        let bearer = format!("Bearer {}", config.access_token.expose_secret());
        let auth = HeaderValue::from_str(&bearer).map_err(|e| LlmError::RequestFailed {
            provider: "openai_codex".to_string(),
            reason: format!("Invalid Authorization header: {e}"),
        })?;
        headers.insert(AUTHORIZATION, auth);
        headers.insert(ACCEPT, HeaderValue::from_static("text/event-stream"));
        headers.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        headers.insert(
            USER_AGENT,
            HeaderValue::from_static(DEFAULT_CODEX_USER_AGENT),
        );
        if let Some(account_id) = config.account_id.as_ref() {
            let header =
                HeaderValue::from_str(account_id).map_err(|e| LlmError::RequestFailed {
                    provider: "openai_codex".to_string(),
                    reason: format!("Invalid ChatGPT-Account-Id header: {e}"),
                })?;
            headers.insert("ChatGPT-Account-Id", header);
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
        format!("{}/responses", self.base_url)
    }

    async fn send_request(&self, body: CodexRequest) -> Result<CodexResponse, LlmError> {
        let response = self
            .client
            .post(self.endpoint_url())
            .json(&body)
            .send()
            .await?;

        if response.status().as_u16() == 401 || response.status().as_u16() == 403 {
            return Err(LlmError::AuthFailed {
                provider: "openai_codex".to_string(),
            });
        }

        if !response.status().is_success() {
            let status = response.status();
            let text = response.text().await.unwrap_or_default();
            return Err(LlmError::RequestFailed {
                provider: "openai_codex".to_string(),
                reason: format!("HTTP {}: {}", status, text),
            });
        }

        parse_sse_response(response).await
    }
}

#[async_trait]
impl LlmProvider for OpenAiCodexProvider {
    fn model_name(&self) -> &str {
        &self.model_name
    }

    fn cost_per_token(&self) -> (Decimal, Decimal) {
        (self.input_cost, self.output_cost)
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let mut request = request;
        let mut messages = std::mem::take(&mut request.messages);
        crate::llm::provider::sanitize_tool_messages(&mut messages);
        let body = CodexRequest::from_completion_request(&self.model_name, request, messages)?;
        let response = self.send_request(body).await?;
        let parsed = parse_codex_response(response);
        Ok(CompletionResponse {
            content: parsed.content.unwrap_or_default(),
            input_tokens: parsed.input_tokens,
            output_tokens: parsed.output_tokens,
            finish_reason: parsed.finish_reason,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
        })
    }

    async fn complete_with_tools(
        &self,
        request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let mut request = request;
        let mut messages = std::mem::take(&mut request.messages);
        crate::llm::provider::sanitize_tool_messages(&mut messages);
        let body = CodexRequest::from_tool_request(&self.model_name, request, messages)?;
        let response = self.send_request(body).await?;
        let parsed = parse_codex_response(response);
        Ok(ToolCompletionResponse {
            content: parsed.content,
            tool_calls: parsed.tool_calls,
            input_tokens: parsed.input_tokens,
            output_tokens: parsed.output_tokens,
            finish_reason: parsed.finish_reason,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
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
struct CodexRequest {
    model: String,
    instructions: String,
    input: Vec<CodexInputItem>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tools: Vec<CodexTool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    tool_choice: Option<String>,
    stream: bool,
    store: bool,
}

impl CodexRequest {
    fn from_completion_request(
        default_model: &str,
        request: CompletionRequest,
        messages: Vec<ChatMessage>,
    ) -> Result<Self, LlmError> {
        let (instructions, input) = split_instructions_and_input(messages)?;
        Ok(Self {
            model: request.model.unwrap_or_else(|| default_model.to_string()),
            instructions,
            input,
            tools: Vec::new(),
            tool_choice: None,
            stream: true,
            store: false,
        })
    }

    fn from_tool_request(
        default_model: &str,
        request: ToolCompletionRequest,
        messages: Vec<ChatMessage>,
    ) -> Result<Self, LlmError> {
        let (instructions, input) = split_instructions_and_input(messages)?;
        Ok(Self {
            model: request.model.unwrap_or_else(|| default_model.to_string()),
            instructions,
            input,
            tools: request.tools.iter().map(convert_tool).collect(),
            tool_choice: request.tool_choice,
            stream: true,
            store: false,
        })
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum CodexInputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        content: Vec<CodexMessageContent>,
    },
    #[serde(rename = "function_call")]
    FunctionCall {
        call_id: String,
        name: String,
        arguments: String,
    },
    #[serde(rename = "function_call_output")]
    FunctionCallOutput { call_id: String, output: String },
}

#[derive(Debug, Serialize)]
struct CodexMessageContent {
    #[serde(rename = "type")]
    kind: &'static str,
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_url: Option<String>,
}

#[derive(Debug, Serialize)]
struct CodexTool {
    #[serde(rename = "type")]
    kind: &'static str,
    name: String,
    description: String,
    parameters: JsonValue,
    strict: bool,
}

fn convert_tool(tool: &ToolDefinition) -> CodexTool {
    CodexTool {
        kind: "function",
        name: tool.name.clone(),
        description: tool.description.clone(),
        parameters: normalize_schema_strict(&tool.parameters),
        strict: true,
    }
}

fn convert_messages_to_codex_items(
    messages: &[ChatMessage],
) -> Result<Vec<CodexInputItem>, LlmError> {
    let mut items = Vec::new();
    for msg in messages {
        match msg.role {
            Role::System | Role::User | Role::Assistant => {
                if !msg.content.is_empty() {
                    let role = match msg.role {
                        Role::System => "system",
                        Role::User => "user",
                        Role::Assistant => "assistant",
                        Role::Tool => unreachable!(),
                    };
                    let content_kind = match msg.role {
                        Role::User => "input_text",
                        Role::Assistant => "output_text",
                        Role::System | Role::Tool => "input_text",
                    };
                    let mut content = vec![CodexMessageContent {
                        kind: content_kind,
                        text: Some(msg.content.clone()),
                        image_url: None,
                    }];
                    if msg.role == Role::User {
                        for part in &msg.content_parts {
                            if let crate::llm::ContentPart::ImageUrl { image_url } = part {
                                content.push(CodexMessageContent {
                                    kind: "input_image",
                                    text: None,
                                    image_url: Some(image_url.url.clone()),
                                });
                            }
                        }
                    }
                    items.push(CodexInputItem::Message {
                        role: role.to_string(),
                        content,
                    });
                }
                if let Some(tool_calls) = msg.tool_calls.as_ref() {
                    for tool_call in tool_calls {
                        items.push(CodexInputItem::FunctionCall {
                            call_id: normalized_tool_call_id(
                                Some(tool_call.id.as_str()),
                                items.len(),
                            ),
                            name: tool_call.name.clone(),
                            arguments: serde_json::to_string(&tool_call.arguments)?,
                        });
                    }
                }
            }
            Role::Tool => {
                let call_id = normalized_tool_call_id(msg.tool_call_id.as_deref(), items.len());
                items.push(CodexInputItem::FunctionCallOutput {
                    call_id,
                    output: msg.content.clone(),
                });
            }
        }
    }

    if items.is_empty() {
        items.push(CodexInputItem::Message {
            role: "user".to_string(),
            content: vec![CodexMessageContent {
                kind: "input_text",
                text: Some("Hello".to_string()),
                image_url: None,
            }],
        });
    }

    Ok(items)
}

fn split_instructions_and_input(
    messages: Vec<ChatMessage>,
) -> Result<(String, Vec<CodexInputItem>), LlmError> {
    let mut instructions = Vec::new();
    let mut non_system = Vec::new();

    for msg in messages {
        if msg.role == Role::System {
            if !msg.content.trim().is_empty() {
                instructions.push(msg.content);
            }
        } else {
            non_system.push(msg);
        }
    }

    Ok((
        instructions.join("\n\n"),
        convert_messages_to_codex_items(&non_system)?,
    ))
}

fn normalized_tool_call_id(raw: Option<&str>, seed: usize) -> String {
    match raw.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => format!("generated_tool_call_{seed}"),
    }
}

#[derive(Debug, serde::Deserialize)]
struct CodexResponse {
    #[serde(default)]
    output: Vec<CodexOutputItem>,
    #[serde(default)]
    usage: Option<CodexUsage>,
    #[serde(default)]
    status: Option<String>,
    #[serde(default)]
    incomplete_details: Option<CodexIncompleteDetails>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexUsage {
    #[serde(default)]
    input_tokens: Option<u32>,
    #[serde(default)]
    output_tokens: Option<u32>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexIncompleteDetails {
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexOutputItem {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    call_id: Option<String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    arguments: Option<String>,
    #[serde(default)]
    content: Vec<CodexOutputContent>,
}

#[derive(Debug, serde::Deserialize)]
struct CodexOutputContent {
    #[serde(rename = "type")]
    kind: String,
    #[serde(default)]
    text: Option<String>,
}

struct ParsedCodexResponse {
    content: Option<String>,
    tool_calls: Vec<ToolCall>,
    input_tokens: u32,
    output_tokens: u32,
    finish_reason: FinishReason,
}

async fn parse_sse_response(response: reqwest::Response) -> Result<CodexResponse, LlmError> {
    let mut stream = response.bytes_stream();
    let mut buffer = String::new();
    let mut aggregate = CodexStreamAggregate::default();

    while let Some(chunk) = stream.next().await {
        let chunk = chunk.map_err(LlmError::Http)?;
        let text = String::from_utf8_lossy(&chunk);
        buffer.push_str(&text);

        while let Some(idx) = buffer.find("\n\n") {
            let frame = buffer[..idx].to_string();
            buffer.drain(..idx + 2);
            handle_sse_frame(&frame, &mut aggregate)?;
        }
    }

    if !buffer.trim().is_empty() {
        handle_sse_frame(&buffer, &mut aggregate)?;
    }

    Ok(aggregate.into_response())
}

#[derive(Default)]
struct CodexStreamAggregate {
    text: String,
    usage: Option<CodexUsage>,
    status: Option<String>,
    incomplete_reason: Option<String>,
    function_calls: std::collections::HashMap<String, PendingFunctionCall>,
}

#[derive(Default)]
struct PendingFunctionCall {
    id: Option<String>,
    call_id: Option<String>,
    name: Option<String>,
    arguments: String,
}

impl CodexStreamAggregate {
    fn into_response(self) -> CodexResponse {
        let mut output = Vec::new();
        if !self.text.is_empty() {
            output.push(CodexOutputItem {
                kind: "message".to_string(),
                id: None,
                call_id: None,
                name: None,
                arguments: None,
                content: vec![CodexOutputContent {
                    kind: "output_text".to_string(),
                    text: Some(self.text),
                }],
            });
        }
        for pending in self.function_calls.into_values() {
            output.push(CodexOutputItem {
                kind: "function_call".to_string(),
                id: pending.id,
                call_id: pending.call_id,
                name: pending.name,
                arguments: Some(pending.arguments),
                content: Vec::new(),
            });
        }
        CodexResponse {
            output,
            usage: self.usage,
            status: self.status,
            incomplete_details: self.incomplete_reason.map(|reason| CodexIncompleteDetails {
                reason: Some(reason),
            }),
        }
    }
}

fn handle_sse_frame(frame: &str, aggregate: &mut CodexStreamAggregate) -> Result<(), LlmError> {
    let mut data_lines = Vec::new();
    for line in frame.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            data_lines.push(rest.trim_start());
        }
    }
    if data_lines.is_empty() {
        return Ok(());
    }

    let data = data_lines.join("\n");
    if data.trim() == "[DONE]" {
        return Ok(());
    }

    let payload: JsonValue = serde_json::from_str(&data)?;
    apply_sse_payload(&payload, aggregate);
    Ok(())
}

fn apply_sse_payload(payload: &JsonValue, aggregate: &mut CodexStreamAggregate) {
    let kind = payload
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    match kind {
        "response.output_text.delta" => {
            if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                aggregate.text.push_str(delta);
            }
        }
        "response.output_text.done" => {
            if aggregate.text.is_empty()
                && let Some(text) = payload.get("text").and_then(|v| v.as_str())
            {
                aggregate.text.push_str(text);
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = payload.get("item") {
                apply_output_item(item, aggregate);
            }
        }
        "response.function_call_arguments.delta" => {
            let item_id = payload
                .get("item_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if !item_id.is_empty() {
                let pending = aggregate.function_calls.entry(item_id).or_default();
                if let Some(delta) = payload.get("delta").and_then(|v| v.as_str()) {
                    pending.arguments.push_str(delta);
                }
            }
        }
        "response.function_call_arguments.done" => {
            let item_id = payload
                .get("item_id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if !item_id.is_empty() {
                let pending = aggregate.function_calls.entry(item_id).or_default();
                if let Some(arguments) = payload.get("arguments").and_then(|v| v.as_str()) {
                    pending.arguments = arguments.to_string();
                }
            }
        }
        "response.completed" => {
            if let Some(response) = payload.get("response") {
                if let Some(usage) = response.get("usage").and_then(decode_usage) {
                    aggregate.usage = Some(usage);
                }
                if let Some(status) = response.get("status").and_then(|v| v.as_str()) {
                    aggregate.status = Some(status.to_string());
                }
                if let Some(reason) = response
                    .get("incomplete_details")
                    .and_then(|d| d.get("reason"))
                    .and_then(|v| v.as_str())
                {
                    aggregate.incomplete_reason = Some(reason.to_string());
                }
            }
        }
        _ => {}
    }
}

fn apply_output_item(item: &JsonValue, aggregate: &mut CodexStreamAggregate) {
    match item.get("type").and_then(|v| v.as_str()) {
        Some("function_call") => {
            let key = item
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_string();
            if key.is_empty() {
                return;
            }
            let pending = aggregate.function_calls.entry(key).or_default();
            pending.id = item.get("id").and_then(|v| v.as_str()).map(str::to_string);
            pending.call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            pending.name = item
                .get("name")
                .and_then(|v| v.as_str())
                .map(str::to_string);
            if let Some(arguments) = item.get("arguments").and_then(|v| v.as_str()) {
                pending.arguments = arguments.to_string();
            }
        }
        Some("message") => {
            if let Some(content) = item.get("content").and_then(|v| v.as_array()) {
                for part in content {
                    if matches!(
                        part.get("type")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default(),
                        "output_text" | "text"
                    ) && let Some(text) = part.get("text").and_then(|v| v.as_str())
                        && !text.is_empty()
                        && aggregate.text.is_empty()
                    {
                        aggregate.text.push_str(text);
                    }
                }
            }
        }
        _ => {}
    }
}

fn decode_usage(value: &JsonValue) -> Option<CodexUsage> {
    Some(CodexUsage {
        input_tokens: value
            .get("input_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
        output_tokens: value
            .get("output_tokens")
            .and_then(|v| v.as_u64())
            .map(|v| v as u32),
    })
}

fn parse_codex_response(response: CodexResponse) -> ParsedCodexResponse {
    let usage = response.usage;
    let mut text_parts = Vec::new();
    let mut tool_calls = Vec::new();

    for item in response.output {
        match item.kind.as_str() {
            "message" => {
                for content in item.content {
                    if matches!(content.kind.as_str(), "output_text" | "text" | "input_text")
                        && let Some(text) = content.text
                    {
                        text_parts.push(text);
                    }
                }
            }
            "function_call" => {
                let args = item
                    .arguments
                    .as_deref()
                    .and_then(|s| serde_json::from_str::<JsonValue>(s).ok())
                    .unwrap_or(JsonValue::Object(serde_json::Map::new()));
                tool_calls.push(ToolCall {
                    id: item
                        .call_id
                        .or(item.id)
                        .unwrap_or_else(|| normalized_tool_call_id(None, tool_calls.len())),
                    name: item.name.unwrap_or_else(|| "unknown".to_string()),
                    arguments: args,
                });
            }
            _ => {}
        }
    }

    let finish_reason = if !tool_calls.is_empty() {
        FinishReason::ToolUse
    } else if response.status.as_deref() == Some("incomplete") {
        match response
            .incomplete_details
            .and_then(|d| d.reason)
            .as_deref()
        {
            Some("max_output_tokens") => FinishReason::Length,
            _ => FinishReason::Unknown,
        }
    } else {
        FinishReason::Stop
    };

    ParsedCodexResponse {
        content: if text_parts.is_empty() {
            None
        } else {
            Some(text_parts.join(""))
        },
        tool_calls,
        input_tokens: usage.as_ref().and_then(|u| u.input_tokens).unwrap_or(0),
        output_tokens: usage.as_ref().and_then(|u| u.output_tokens).unwrap_or(0),
        finish_reason,
    }
}

use secrecy::ExposeSecret;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_messages_and_tool_results_to_codex_items() {
        let messages = vec![
            ChatMessage::system("sys"),
            ChatMessage::assistant_with_tool_calls(
                Some("thinking".to_string()),
                vec![ToolCall {
                    id: "call_1".to_string(),
                    name: "search".to_string(),
                    arguments: serde_json::json!({"q": "test"}),
                }],
            ),
            ChatMessage::tool_result("call_1", "search", "result"),
        ];

        let items = convert_messages_to_codex_items(&messages).expect("convert");
        assert!(matches!(items[0], CodexInputItem::Message { .. }));
        assert!(matches!(items[1], CodexInputItem::Message { .. }));
        assert!(matches!(items[2], CodexInputItem::FunctionCall { .. }));
        assert!(matches!(
            items[3],
            CodexInputItem::FunctionCallOutput { .. }
        ));
    }

    #[test]
    fn assistant_history_uses_output_text_content_type() {
        let messages = vec![ChatMessage::user("hello"), ChatMessage::assistant("world")];

        let items = convert_messages_to_codex_items(&messages).expect("convert");
        match &items[0] {
            CodexInputItem::Message { role, content } => {
                assert_eq!(role, "user");
                assert_eq!(content[0].kind, "input_text");
            }
            _ => panic!("expected user message"),
        }
        match &items[1] {
            CodexInputItem::Message { role, content } => {
                assert_eq!(role, "assistant");
                assert_eq!(content[0].kind, "output_text");
            }
            _ => panic!("expected assistant message"),
        }
    }

    #[test]
    fn splits_system_messages_into_instructions() {
        let messages = vec![
            ChatMessage::system("sys one"),
            ChatMessage::system("sys two"),
            ChatMessage::user("hello"),
        ];

        let (instructions, input) = split_instructions_and_input(messages).expect("split");
        assert_eq!(instructions, "sys one\n\nsys two");
        assert_eq!(input.len(), 1);
        assert!(matches!(input[0], CodexInputItem::Message { .. }));
    }

    #[test]
    fn parses_text_and_tool_calls_from_codex_response() {
        let response = CodexResponse {
            output: vec![
                CodexOutputItem {
                    kind: "message".to_string(),
                    id: None,
                    call_id: None,
                    name: None,
                    arguments: None,
                    content: vec![CodexOutputContent {
                        kind: "output_text".to_string(),
                        text: Some("hello".to_string()),
                    }],
                },
                CodexOutputItem {
                    kind: "function_call".to_string(),
                    id: Some("fc_1".to_string()),
                    call_id: Some("call_1".to_string()),
                    name: Some("search".to_string()),
                    arguments: Some("{\"q\":\"test\"}".to_string()),
                    content: Vec::new(),
                },
            ],
            usage: Some(CodexUsage {
                input_tokens: Some(10),
                output_tokens: Some(5),
            }),
            status: Some("completed".to_string()),
            incomplete_details: None,
        };

        let parsed = parse_codex_response(response);
        assert_eq!(parsed.content.as_deref(), Some("hello"));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "search");
        assert_eq!(parsed.finish_reason, FinishReason::ToolUse);
    }

    #[test]
    fn aggregates_sse_payloads() {
        let mut aggregate = CodexStreamAggregate::default();

        apply_sse_payload(
            &serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "hello"
            }),
            &mut aggregate,
        );
        apply_sse_payload(
            &serde_json::json!({
                "type": "response.output_item.added",
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_1",
                    "name": "search",
                    "arguments": ""
                }
            }),
            &mut aggregate,
        );
        apply_sse_payload(
            &serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "delta": "{\"q\":"
            }),
            &mut aggregate,
        );
        apply_sse_payload(
            &serde_json::json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "delta": "\"test\"}"
            }),
            &mut aggregate,
        );
        apply_sse_payload(
            &serde_json::json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "usage": { "input_tokens": 7, "output_tokens": 3 }
                }
            }),
            &mut aggregate,
        );

        let response = aggregate.into_response();
        let parsed = parse_codex_response(response);
        assert_eq!(parsed.content.as_deref(), Some("hello"));
        assert_eq!(parsed.tool_calls.len(), 1);
        assert_eq!(parsed.tool_calls[0].name, "search");
        assert_eq!(
            parsed.tool_calls[0].arguments,
            serde_json::json!({"q": "test"})
        );
        assert_eq!(parsed.input_tokens, 7);
        assert_eq!(parsed.output_tokens, 3);
    }

    #[test]
    fn does_not_duplicate_text_when_message_item_repeats_streamed_text() {
        let mut aggregate = CodexStreamAggregate::default();

        apply_sse_payload(
            &serde_json::json!({
                "type": "response.output_text.delta",
                "delta": "hello"
            }),
            &mut aggregate,
        );
        apply_sse_payload(
            &serde_json::json!({
                "type": "response.output_item.done",
                "item": {
                    "type": "message",
                    "content": [
                        {
                            "type": "output_text",
                            "text": "hello"
                        }
                    ]
                }
            }),
            &mut aggregate,
        );

        let response = aggregate.into_response();
        let parsed = parse_codex_response(response);
        assert_eq!(parsed.content.as_deref(), Some("hello"));
    }
}
