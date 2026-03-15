use std::collections::HashMap;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use reqwest::StatusCode;
use serde::Serialize;
use serde_json::{Value, json};

use crate::model::openai_compat::OpenAiCompatibleConfig;
use crate::model::{
    AccumulationMode, ExchangeAccumulator, ModelEngineError, ModelEvent, ModelExchangeRequest,
    ModelExchangeResult, ModelMessage, ModelRunner, ModelUsage, RawFrame, RawModelTrace,
    ReasoningMode, TraceOutcome, TransportKind,
};

#[derive(Debug)]
pub struct OpenAiResponsesEngine {
    client: reqwest::Client,
    config: OpenAiCompatibleConfig,
}

impl OpenAiResponsesEngine {
    pub fn new(config: OpenAiCompatibleConfig) -> Result<Self, anyhow::Error> {
        let client = config.build_client(true)?;
        Ok(Self { client, config })
    }

    fn endpoint(&self) -> String {
        self.config.endpoint("responses")
    }

    fn build_payload(&self, request: &ModelExchangeRequest) -> Value {
        let (instructions, input) = split_instructions_and_input(&request.messages);
        let mut payload = json!({
            "model": request.model,
            "instructions": instructions,
            "input": input,
            "stream": request.stream,
            "store": false,
        });

        if !request.tools.is_empty() {
            payload["tools"] = Value::Array(
                request
                    .tools
                    .iter()
                    .map(convert_tool_definition)
                    .collect::<Vec<_>>(),
            );
        }
        if let Some(temperature) = request.temperature {
            payload["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = request.max_tokens {
            payload["max_output_tokens"] = json!(max_tokens);
        }
        if let Some(response_format) = &request.response_format {
            payload["text"] = json!({ "format": response_format });
        }
        if let Some(extra) = request.extra.as_object() {
            if let Some(target) = payload.as_object_mut() {
                for (key, value) in extra {
                    target.insert(key.clone(), value.clone());
                }
            }
        }
        payload
    }

    fn new_accumulator(
        &self,
        model: &str,
        accumulation_mode: AccumulationMode,
    ) -> ExchangeAccumulator {
        ExchangeAccumulator::new(model.to_string(), accumulation_mode)
    }

    fn base_raw_trace(
        &self,
        request_body: Value,
        provider_request_id: Option<String>,
        transport_kind: TransportKind,
        accumulation_mode: AccumulationMode,
        reasoning_mode: ReasoningMode,
    ) -> RawModelTrace {
        RawModelTrace {
            request_body,
            response_body: None,
            raw_frames: Vec::new(),
            provider_request_id,
            transport_kind,
            accumulation_mode,
            reasoning_mode,
        }
    }

    fn reduced_error_result(
        &self,
        request: &ModelExchangeRequest,
        started_at: chrono::DateTime<chrono::Utc>,
        request_body: Value,
        provider_request_id: Option<String>,
        transport_kind: TransportKind,
        message: String,
        response_body: Option<Value>,
        accumulation_mode: AccumulationMode,
        reasoning_mode: ReasoningMode,
    ) -> ModelExchangeResult {
        let completed_at = Utc::now();
        ModelExchangeResult {
            model: request.model.clone(),
            request_started_at: started_at,
            request_completed_at: completed_at,
            content: None,
            reasoning: None,
            tool_calls: Vec::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            raw_trace: RawModelTrace {
                request_body,
                response_body,
                raw_frames: Vec::new(),
                provider_request_id,
                transport_kind,
                accumulation_mode,
                reasoning_mode,
            },
            normalized_events: vec![
                ModelEvent::ExchangeStarted,
                ModelEvent::Failed {
                    message: message.clone(),
                },
            ],
            outcome: TraceOutcome::TransportError,
            error_summary: Some(message),
        }
    }

    fn decode_json_response(
        &self,
        request: &ModelExchangeRequest,
        started_at: chrono::DateTime<chrono::Utc>,
        request_body: Value,
        response_body: Value,
        provider_request_id: Option<String>,
    ) -> ModelExchangeResult {
        let completed_at = Utc::now();
        let mut events = vec![ModelEvent::ExchangeStarted];
        let mut accumulator = self.new_accumulator(&request.model, AccumulationMode::FullSnapshot);
        accumulator.push(&events[0]);
        let mut reasoning_mode = ReasoningMode::Unknown;
        for event in decode_responses_json(&response_body, &mut reasoning_mode) {
            accumulator.push(&event);
            events.push(event);
        }
        accumulator.build(
            started_at,
            completed_at,
            RawModelTrace {
                request_body,
                response_body: Some(response_body),
                raw_frames: Vec::new(),
                provider_request_id,
                transport_kind: TransportKind::HttpJson,
                accumulation_mode: AccumulationMode::FullSnapshot,
                reasoning_mode,
            },
            events,
        )
    }

    async fn decode_sse_response(
        &self,
        request: &ModelExchangeRequest,
        started_at: chrono::DateTime<chrono::Utc>,
        request_body: Value,
        provider_request_id: Option<String>,
        response: reqwest::Response,
    ) -> Result<ModelExchangeResult, ModelEngineError> {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let mut raw_trace = self.base_raw_trace(
            request_body,
            provider_request_id,
            TransportKind::HttpSse,
            AccumulationMode::DeltaPlusFinal,
            reasoning_mode,
        );
        let mut accumulator = self.new_accumulator(&request.model, AccumulationMode::DeltaPlusFinal);
        let mut events = vec![ModelEvent::ExchangeStarted];
        accumulator.push(&events[0]);

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut frame_index = 0usize;
        let mut saw_done = false;
        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(chunk) => chunk,
                Err(error) => {
                    let completed_at = Utc::now();
                    let exchange = accumulator.build(started_at, completed_at, raw_trace, events);
                    return Err(ModelEngineError::TransportFailure {
                        message: error.to_string(),
                        exchange: Box::new(ModelExchangeResult {
                            outcome: TraceOutcome::TransportError,
                            error_summary: Some(error.to_string()),
                            ..exchange
                        }),
                    });
                }
            };
            buffer.push_str(&String::from_utf8_lossy(&chunk));
            while let Some((block, rest)) = take_sse_block(&buffer) {
                buffer = rest;
                let Some(data) = parse_sse_data(&block) else {
                    continue;
                };
                if data == "[DONE]" {
                    saw_done = true;
                    continue;
                }
                let frame_value = match serde_json::from_str::<Value>(&data) {
                    Ok(value) => value,
                    Err(error) => {
                        let completed_at = Utc::now();
                        events.push(ModelEvent::Failed {
                            message: format!("failed to parse SSE frame: {error}"),
                        });
                        let exchange =
                            accumulator.build(started_at, completed_at, raw_trace, events);
                        return Err(ModelEngineError::TransportFailure {
                            message: format!("failed to parse SSE frame: {error}"),
                            exchange: Box::new(ModelExchangeResult {
                                outcome: TraceOutcome::TransportError,
                                error_summary: Some(error.to_string()),
                                ..exchange
                            }),
                        });
                    }
                };
                raw_trace.raw_frames.push(RawFrame {
                    sequence: frame_index,
                    data: frame_value.clone(),
                });
                frame_index += 1;
                for event in decode_responses_stream_frame(&frame_value, &mut reasoning_mode) {
                    accumulator.push(&event);
                    events.push(event);
                }
            }
        }

        if !buffer.trim().is_empty() {
            if let Ok(frame_value) = serde_json::from_str::<Value>(buffer.trim()) {
                raw_trace.raw_frames.push(RawFrame {
                    sequence: frame_index,
                    data: frame_value.clone(),
                });
                for event in decode_responses_stream_frame(&frame_value, &mut reasoning_mode) {
                    accumulator.push(&event);
                    events.push(event);
                }
            }
        }

        if !saw_done && !events.iter().any(|event| matches!(event, ModelEvent::Completed { .. })) {
            let event = ModelEvent::Completed {
                finish_reason: None,
            };
            accumulator.push(&event);
            events.push(event);
        }

        let completed_at = Utc::now();
        raw_trace.reasoning_mode = reasoning_mode;
        Ok(accumulator.build(started_at, completed_at, raw_trace, events))
    }
}

#[async_trait]
impl ModelRunner for OpenAiResponsesEngine {
    async fn run(
        &self,
        request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, ModelEngineError> {
        let started_at = Utc::now();
        let payload = self.build_payload(&request);
        let response = self
            .client
            .post(self.endpoint())
            .header(CONTENT_TYPE, "application/json")
            .json(&payload)
            .send()
            .await;

        let response = match response {
            Ok(response) => response,
            Err(error) => {
                let exchange = self.reduced_error_result(
                    &request,
                    started_at,
                    payload,
                    None,
                    TransportKind::HttpJson,
                    format!("{} transport failure: {error}", self.config.provider_name),
                    None,
                    AccumulationMode::FullSnapshot,
                    ReasoningMode::Unknown,
                );
                return Err(ModelEngineError::TransportFailure {
                    message: error.to_string(),
                    exchange: Box::new(exchange),
                });
            }
        };

        let provider_request_id = OpenAiCompatibleConfig::provider_request_id(response.headers());
        if response.status() != StatusCode::OK {
            let status = response.status().as_u16();
            let body_text = response.text().await.unwrap_or_default();
            let body_json =
                serde_json::from_str(&body_text).unwrap_or_else(|_| json!({ "body": body_text }));
            let exchange = self.reduced_error_result(
                &request,
                started_at,
                payload,
                provider_request_id,
                TransportKind::HttpJson,
                format!("{} returned HTTP {status}", self.config.provider_name),
                Some(body_json),
                AccumulationMode::FullSnapshot,
                ReasoningMode::Unknown,
            );
            return Err(ModelEngineError::HttpFailure {
                status,
                message: format!("{} returned HTTP {status}", self.config.provider_name),
                exchange: Box::new(exchange),
            });
        }

        if request.stream {
            self.decode_sse_response(&request, started_at, payload, provider_request_id, response)
                .await
        } else {
            let response_body: Value =
                response
                    .json()
                    .await
                    .map_err(|error| ModelEngineError::TransportFailure {
                        message: error.to_string(),
                        exchange: Box::new(self.reduced_error_result(
                            &request,
                            started_at,
                            payload.clone(),
                            provider_request_id.clone(),
                            TransportKind::HttpJson,
                            format!("{} transport failure: {error}", self.config.provider_name),
                            None,
                            AccumulationMode::FullSnapshot,
                            ReasoningMode::Unknown,
                        )),
                    })?;
            Ok(self.decode_json_response(
                &request,
                started_at,
                payload,
                response_body,
                provider_request_id,
            ))
        }
    }
}

#[derive(Debug, Serialize)]
#[serde(tag = "type")]
enum ResponseInputItem {
    #[serde(rename = "message")]
    Message {
        role: String,
        content: Vec<ResponseContentItem>,
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
struct ResponseContentItem {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

fn split_instructions_and_input(messages: &[ModelMessage]) -> (String, Vec<Value>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        match message.role.as_str() {
            "system" => {
                if let Some(content) = message.content.as_deref()
                    && !content.trim().is_empty()
                {
                    instructions.push(content.to_string());
                }
            }
            "tool" => {
                input.push(
                    serde_json::to_value(ResponseInputItem::FunctionCallOutput {
                        call_id: normalized_tool_call_id(message.tool_call_id.as_deref(), index),
                        output: message.content.clone().unwrap_or_default(),
                    })
                    .expect("function_call_output should serialize"),
                );
            }
            role => {
                if let Some(content) = message.content.as_deref()
                    && !content.is_empty()
                {
                    input.push(
                        serde_json::to_value(ResponseInputItem::Message {
                            role: role.to_string(),
                            content: vec![ResponseContentItem {
                                kind: "input_text",
                                text: content.to_string(),
                            }],
                        })
                        .expect("message item should serialize"),
                    );
                }
                if let Some(tool_calls) = &message.tool_calls {
                    for tool_call in tool_calls {
                        input.push(
                            serde_json::to_value(ResponseInputItem::FunctionCall {
                                call_id: normalized_tool_call_id(Some(&tool_call.id), index),
                                name: tool_call.function.name.clone(),
                                arguments: tool_call.function.arguments.clone(),
                            })
                            .expect("function_call should serialize"),
                        );
                    }
                }
            }
        }
    }

    if input.is_empty() {
        input.push(json!({
            "type": "message",
            "role": "user",
            "content": [{ "type": "input_text", "text": "Hello" }]
        }));
    }

    (instructions.join("\n\n"), input)
}

fn normalized_tool_call_id(raw: Option<&str>, seed: usize) -> String {
    match raw.map(str::trim).filter(|id| !id.is_empty()) {
        Some(id) => id.to_string(),
        None => format!("generated_tool_call_{seed}"),
    }
}

fn convert_tool_definition(tool: &Value) -> Value {
    if tool.get("type").and_then(Value::as_str) == Some("function")
        && let Some(function) = tool.get("function")
    {
        return json!({
            "type": "function",
            "name": function.get("name").cloned().unwrap_or(Value::String(String::new())),
            "description": function.get("description").cloned().unwrap_or(Value::Null),
            "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {} })),
            "strict": true,
        });
    }
    tool.clone()
}

fn decode_responses_json(response_body: &Value, reasoning_mode: &mut ReasoningMode) -> Vec<ModelEvent> {
    let mut events = Vec::new();

    if let Some(error_message) = response_body
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        events.push(ModelEvent::Failed {
            message: error_message.to_string(),
        });
    }

    if let Some(output) = response_body.get("output").and_then(Value::as_array) {
        let mut state = ResponseDecodeState::default();
        for item in output {
            append_output_item_events(item, &mut state, &mut events, reasoning_mode);
        }
    } else if let Some(output_text) = response_body.get("output_text").and_then(Value::as_str) {
        events.push(ModelEvent::TextSnapshot {
            text: output_text.to_string(),
        });
    }

    if let Some(usage) = response_body.get("usage") {
        events.push(ModelEvent::UsageUpdated {
            usage: decode_usage(usage),
        });
    }

    let finish_reason = response_body
        .get("incomplete_details")
        .and_then(|details| details.get("reason"))
        .and_then(Value::as_str)
        .map(ToString::to_string)
        .or_else(|| {
            response_body
                .get("status")
                .and_then(Value::as_str)
                .filter(|status| *status != "completed")
                .map(ToString::to_string)
        });

    events.push(ModelEvent::Completed { finish_reason });
    events
}

fn decode_responses_stream_frame(
    frame: &Value,
    reasoning_mode: &mut ReasoningMode,
) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    let mut state = ResponseDecodeState::default();

    if let Some(error_message) = frame
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        events.push(ModelEvent::Failed {
            message: error_message.to_string(),
        });
    }

    let kind = frame.get("type").and_then(Value::as_str).unwrap_or_default();
    match kind {
        "response.output_text.delta" => {
            if let Some(delta) = frame.get("delta").and_then(Value::as_str) {
                events.push(ModelEvent::TextDelta {
                    text: delta.to_string(),
                });
            }
        }
        "response.output_text.done" => {
            if let Some(text) = frame.get("text").and_then(Value::as_str) {
                events.push(ModelEvent::TextFinal {
                    text: text.to_string(),
                });
            }
        }
        "response.output_item.added" | "response.output_item.done" => {
            if let Some(item) = frame.get("item") {
                append_output_item_events(item, &mut state, &mut events, reasoning_mode);
            }
        }
        "response.function_call_arguments.delta" => {
            if let Some(item_id) = frame.get("item_id").and_then(Value::as_str) {
                events.push(ModelEvent::ToolCallStarted {
                    key: item_id.to_string(),
                    id: frame
                        .get("call_id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                });
                if let Some(delta) = frame.get("delta").and_then(Value::as_str) {
                    events.push(ModelEvent::ToolCallArgumentsDelta {
                        key: item_id.to_string(),
                        text: delta.to_string(),
                    });
                }
            }
        }
        "response.function_call_arguments.done" => {
            if let Some(item_id) = frame.get("item_id").and_then(Value::as_str) {
                if let Some(arguments) = frame.get("arguments").and_then(Value::as_str) {
                    events.push(ModelEvent::ToolCallArgumentsDelta {
                        key: item_id.to_string(),
                        text: arguments.to_string(),
                    });
                }
                events.push(ModelEvent::ToolCallFinished {
                    key: item_id.to_string(),
                });
            }
        }
        "response.completed" => {
            if let Some(response) = frame.get("response") {
                if let Some(usage) = response.get("usage") {
                    events.push(ModelEvent::UsageUpdated {
                        usage: decode_usage(usage),
                    });
                }
                if let Some(output) = response.get("output").and_then(Value::as_array) {
                    for item in output {
                        append_output_item_events(item, &mut state, &mut events, reasoning_mode);
                    }
                }
                let finish_reason = response
                    .get("incomplete_details")
                    .and_then(|details| details.get("reason"))
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .or_else(|| {
                        response
                            .get("status")
                            .and_then(Value::as_str)
                            .filter(|status| *status != "completed")
                            .map(ToString::to_string)
                    });
                events.push(ModelEvent::Completed { finish_reason });
            }
        }
        _ => {}
    }

    events
}

#[derive(Default)]
struct ResponseDecodeState {
    text_snapshots: HashMap<String, String>,
    reasoning_snapshots: HashMap<String, String>,
}

fn append_output_item_events(
    item: &Value,
    state: &mut ResponseDecodeState,
    events: &mut Vec<ModelEvent>,
    reasoning_mode: &mut ReasoningMode,
) {
    match item.get("type").and_then(Value::as_str).unwrap_or_default() {
        "message" => append_message_item_events(item, state, events, reasoning_mode),
        "reasoning" => {
            *reasoning_mode = ReasoningMode::Structured;
            let text = item
                .get("summary")
                .and_then(Value::as_array)
                .map(|parts| {
                    parts
                        .iter()
                        .filter_map(|part| part.get("text").and_then(Value::as_str))
                        .collect::<Vec<_>>()
                        .join("\n")
                })
                .filter(|text| !text.is_empty())
                .or_else(|| {
                    item.get("content").and_then(Value::as_array).map(|parts| {
                        parts
                            .iter()
                            .filter_map(|part| part.get("text").and_then(Value::as_str))
                            .collect::<Vec<_>>()
                            .join("\n")
                    })
                })
                .filter(|text| !text.is_empty());
            if let Some(text) = text {
                events.push(ModelEvent::ReasoningSnapshot { text });
            }
        }
        "function_call" => {
            let key = item
                .get("id")
                .and_then(Value::as_str)
                .map(ToString::to_string)
                .or_else(|| item.get("call_id").and_then(Value::as_str).map(ToString::to_string))
                .unwrap_or_else(|| "function_call".to_string());
            events.push(ModelEvent::ToolCallStarted {
                key: key.clone(),
                id: item
                    .get("call_id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string),
            });
            if let Some(name) = item.get("name").and_then(Value::as_str) {
                events.push(ModelEvent::ToolCallNameDelta {
                    key: key.clone(),
                    text: name.to_string(),
                });
            }
            if let Some(arguments) = item.get("arguments").and_then(Value::as_str) {
                events.push(ModelEvent::ToolCallArgumentsDelta {
                    key: key.clone(),
                    text: arguments.to_string(),
                });
            }
            if item.get("status").and_then(Value::as_str) != Some("in_progress") {
                events.push(ModelEvent::ToolCallFinished { key });
            }
        }
        _ => {}
    }
}

fn append_message_item_events(
    item: &Value,
    state: &mut ResponseDecodeState,
    events: &mut Vec<ModelEvent>,
    reasoning_mode: &mut ReasoningMode,
) {
    let message_id = item
        .get("id")
        .and_then(Value::as_str)
        .unwrap_or("message")
        .to_string();
    let Some(content) = item.get("content").and_then(Value::as_array) else {
        return;
    };

    for part in content {
        match part.get("type").and_then(Value::as_str).unwrap_or_default() {
            "output_text" => {
                let text = part
                    .get("text")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                if text.contains("<think") || text.contains("<thinking") || text.contains("<thought")
                {
                    *reasoning_mode = ReasoningMode::InlineTagged;
                }
                let previous = state.text_snapshots.insert(message_id.clone(), text.clone());
                if previous.is_some() {
                    events.push(ModelEvent::TextFinal { text });
                } else {
                    events.push(ModelEvent::TextSnapshot { text });
                }
            }
            "refusal" => {
                if let Some(text) = part.get("refusal").and_then(Value::as_str) {
                    events.push(ModelEvent::TextSnapshot {
                        text: text.to_string(),
                    });
                }
            }
            "reasoning_text" | "summary_text" => {
                *reasoning_mode = ReasoningMode::Structured;
                if let Some(text) = part.get("text").and_then(Value::as_str) {
                    let previous = state
                        .reasoning_snapshots
                        .insert(message_id.clone(), text.to_string());
                    if previous.is_some() {
                        events.push(ModelEvent::ReasoningFinal {
                            text: text.to_string(),
                        });
                    } else {
                        events.push(ModelEvent::ReasoningSnapshot {
                            text: text.to_string(),
                        });
                    }
                }
            }
            _ => {}
        }
    }
}

fn decode_usage(usage: &Value) -> ModelUsage {
    ModelUsage {
        input_tokens: usage
            .get("input_tokens")
            .or_else(|| usage.get("prompt_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("output_tokens")
            .or_else(|| usage.get("completion_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        cache_read_input_tokens: usage
            .get("input_tokens_details")
            .or_else(|| usage.get("prompt_tokens_details"))
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        cache_creation_input_tokens: 0,
    }
}

fn take_sse_block(buffer: &str) -> Option<(String, String)> {
    if let Some(index) = buffer.find("\n\n") {
        return Some((buffer[..index].to_string(), buffer[index + 2..].to_string()));
    }
    if let Some(index) = buffer.find("\r\n\r\n") {
        return Some((buffer[..index].to_string(), buffer[index + 4..].to_string()));
    }
    None
}

fn parse_sse_data(block: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            lines.push(rest.trim_start().to_string());
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        convert_tool_definition, decode_responses_json, decode_responses_stream_frame,
        split_instructions_and_input,
    };
    use crate::model::{ModelEvent, ModelMessage, ModelToolCallMessage, ModelToolFunctionMessage, ReasoningMode};

    #[test]
    fn translates_messages_to_instructions_and_input() {
        let (instructions, input) = split_instructions_and_input(&[
            ModelMessage {
                role: "system".to_string(),
                content: Some("be careful".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
            ModelMessage {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
        ]);
        assert_eq!(instructions, "be careful");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn translates_tool_calls_and_tool_outputs() {
        let (_instructions, input) = split_instructions_and_input(&[
            ModelMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![ModelToolCallMessage {
                    id: "call-1".to_string(),
                    kind: "function".to_string(),
                    function: ModelToolFunctionMessage {
                        name: "echo".to_string(),
                        arguments: "{\"message\":\"hi\"}".to_string(),
                    },
                }]),
                tool_call_id: None,
            },
            ModelMessage {
                role: "tool".to_string(),
                content: Some("{\"message\":\"hi\"}".to_string()),
                tool_calls: None,
                tool_call_id: Some("call-1".to_string()),
            },
        ]);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call-1");
    }

    #[test]
    fn converts_chat_style_tools_to_responses_tools() {
        let converted = convert_tool_definition(&json!({
            "type": "function",
            "function": {
                "name": "echo",
                "description": "Prints a message",
                "parameters": { "type": "object", "properties": { "message": { "type": "string" } } }
            }
        }));
        assert_eq!(converted["type"], "function");
        assert_eq!(converted["name"], "echo");
        assert_eq!(converted["strict"], true);
    }

    #[test]
    fn decodes_responses_json_tool_calls() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_responses_json(
            &json!({
                "output": [{
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call-1",
                    "name": "echo",
                    "arguments": "{\"message\":\"hi\"}",
                    "status": "completed"
                }],
                "usage": { "input_tokens": 5, "output_tokens": 2 }
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallArgumentsDelta { key, text } if key == "fc_1" && text == "{\"message\":\"hi\"}"
        )));
    }

    #[test]
    fn decodes_responses_sse_frames() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_responses_stream_frame(
            &json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "delta": "{\"path\":\"README.md\"}"
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallArgumentsDelta { key, text } if key == "fc_1" && text == "{\"path\":\"README.md\"}"
        )));
    }

    #[test]
    fn decodes_responses_completion_frame() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_responses_stream_frame(
            &json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_1",
                        "content": [{ "type": "output_text", "text": "done" }]
                    }],
                    "usage": { "input_tokens": 7, "output_tokens": 3 }
                }
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::TextSnapshot { text } if text == "done"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Completed { finish_reason } if finish_reason.is_none()
        )));
    }
}
