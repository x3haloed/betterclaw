use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::header::CONTENT_TYPE;
use reqwest::{Client, StatusCode};
use serde_json::{Value, json};

use crate::model::{
    ExchangeAccumulator, ModelEngineError, ModelEvent, ModelExchangeRequest, ModelExchangeResult,
    ModelRunner, ModelUsage, RawFrame, RawModelTrace, TransportKind, TraceOutcome,
};

#[derive(Debug, Clone)]
pub struct OpenAiChatCompletionsConfig {
    pub base_url: String,
    pub timeout: Duration,
}

impl Default for OpenAiChatCompletionsConfig {
    fn default() -> Self {
        Self {
            base_url: "http://localhost:1234/v1".to_string(),
            timeout: Duration::from_secs(120),
        }
    }
}

#[derive(Debug)]
pub struct OpenAiChatCompletionsEngine {
    client: Client,
    config: OpenAiChatCompletionsConfig,
}

impl OpenAiChatCompletionsEngine {
    pub fn new(config: OpenAiChatCompletionsConfig) -> Result<Self, anyhow::Error> {
        let client = Client::builder().timeout(config.timeout).build()?;
        Ok(Self { client, config })
    }

    fn endpoint(&self) -> String {
        format!(
            "{}/chat/completions",
            self.config.base_url.trim_end_matches('/')
        )
    }

    fn build_payload(&self, request: &ModelExchangeRequest) -> Value {
        let mut payload = json!({
            "model": request.model,
            "messages": request.messages,
            "stream": request.stream,
        });
        if !request.tools.is_empty() {
            payload["tools"] = Value::Array(request.tools.clone());
        }
        if let Some(temperature) = request.temperature {
            payload["temperature"] = json!(temperature);
        }
        if let Some(max_tokens) = request.max_tokens {
            payload["max_tokens"] = json!(max_tokens);
        }
        if let Some(response_format) = &request.response_format {
            payload["response_format"] = response_format.clone();
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

    fn new_accumulator(&self, model: &str) -> ExchangeAccumulator {
        ExchangeAccumulator::new(model.to_string())
    }

    fn base_raw_trace(
        &self,
        request_body: Value,
        provider_request_id: Option<String>,
        transport_kind: TransportKind,
    ) -> RawModelTrace {
        RawModelTrace {
            request_body,
            response_body: None,
            raw_frames: Vec::new(),
            provider_request_id,
            transport_kind,
        }
    }

    fn provider_request_id(headers: &reqwest::header::HeaderMap) -> Option<String> {
        headers
            .get("x-request-id")
            .or_else(|| headers.get("x-lmstudio-request-id"))
            .and_then(|value| value.to_str().ok())
            .map(ToString::to_string)
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
        let mut accumulator = self.new_accumulator(&request.model);
        accumulator.push(&events[0]);
        for event in decode_openai_response_json(&response_body) {
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
        let mut raw_trace =
            self.base_raw_trace(request_body, provider_request_id, TransportKind::HttpSse);
        let mut accumulator = self.new_accumulator(&request.model);
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
                    let exchange = accumulator.build(
                        started_at,
                        completed_at,
                        raw_trace,
                        events,
                    );
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
                        let exchange = accumulator.build(
                            started_at,
                            completed_at,
                            raw_trace,
                            events,
                        );
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
                for event in decode_openai_stream_frame(&frame_value) {
                    accumulator.push(&event);
                    events.push(event);
                }
            }
        }

        if !saw_done {
            let event = ModelEvent::Completed { finish_reason: None };
            accumulator.push(&event);
            events.push(event);
        }
        let completed_at = Utc::now();
        Ok(accumulator.build(started_at, completed_at, raw_trace, events))
    }
}

#[async_trait]
impl ModelRunner for OpenAiChatCompletionsEngine {
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
                    error.to_string(),
                    None,
                );
                return Err(ModelEngineError::TransportFailure {
                    message: error.to_string(),
                    exchange: Box::new(exchange),
                });
            }
        };

        let provider_request_id = Self::provider_request_id(response.headers());
        if response.status() != StatusCode::OK {
            let status = response.status().as_u16();
            let body_text = response.text().await.unwrap_or_default();
            let body_json = serde_json::from_str(&body_text).unwrap_or_else(|_| json!({ "body": body_text }));
            let exchange = self.reduced_error_result(
                &request,
                started_at,
                payload,
                provider_request_id,
                TransportKind::HttpJson,
                format!("provider returned HTTP {status}"),
                Some(body_json),
            );
            return Err(ModelEngineError::HttpFailure {
                status,
                message: format!("provider returned HTTP {status}"),
                exchange: Box::new(exchange),
            });
        }

        if request.stream {
            self.decode_sse_response(&request, started_at, payload, provider_request_id, response)
                .await
        } else {
            let response_body: Value = response.json().await.map_err(|error| {
                ModelEngineError::TransportFailure {
                    message: error.to_string(),
                    exchange: Box::new(self.reduced_error_result(
                        &request,
                        started_at,
                        payload.clone(),
                        provider_request_id.clone(),
                        TransportKind::HttpJson,
                        error.to_string(),
                        None,
                    )),
                }
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

fn decode_openai_response_json(response_body: &Value) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    let choice = response_body
        .get("choices")
        .and_then(Value::as_array)
        .and_then(|choices| choices.first());
    if let Some(choice) = choice {
        if let Some(content) = choice
            .get("message")
            .and_then(|message| message.get("content"))
            .and_then(Value::as_str)
        {
            events.push(ModelEvent::TextDelta {
                text: content.to_string(),
            });
        }
        if let Some(tool_calls) = choice
            .get("message")
            .and_then(|message| message.get("tool_calls"))
            .and_then(Value::as_array)
        {
            for (index, tool_call) in tool_calls.iter().enumerate() {
                let key = tool_call
                    .get("id")
                    .and_then(Value::as_str)
                    .map(ToString::to_string)
                    .unwrap_or_else(|| index.to_string());
                events.push(ModelEvent::ToolCallStarted {
                    key: key.clone(),
                    id: tool_call
                        .get("id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string),
                });
                if let Some(name) = tool_call
                    .get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(Value::as_str)
                {
                    events.push(ModelEvent::ToolCallNameDelta {
                        key: key.clone(),
                        text: name.to_string(),
                    });
                }
                if let Some(arguments) = tool_call
                    .get("function")
                    .and_then(|function| function.get("arguments"))
                    .and_then(Value::as_str)
                {
                    events.push(ModelEvent::ToolCallArgumentsDelta {
                        key: key.clone(),
                        text: arguments.to_string(),
                    });
                }
                events.push(ModelEvent::ToolCallFinished { key });
            }
        }
        events.push(ModelEvent::Completed {
            finish_reason: choice
                .get("finish_reason")
                .and_then(Value::as_str)
                .map(ToString::to_string),
        });
    }
    if let Some(usage) = response_body.get("usage") {
        events.push(ModelEvent::UsageUpdated {
            usage: decode_usage(usage),
        });
    }
    events
}

fn decode_openai_stream_frame(frame: &Value) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    if let Some(usage) = frame.get("usage") {
        events.push(ModelEvent::UsageUpdated {
            usage: decode_usage(usage),
        });
    }
    let Some(choices) = frame.get("choices").and_then(Value::as_array) else {
        return events;
    };
    for choice in choices {
        if let Some(delta) = choice.get("delta") {
            if let Some(content) = delta.get("content").and_then(Value::as_str) {
                events.push(ModelEvent::TextDelta {
                    text: content.to_string(),
                });
            }
            if let Some(reasoning) = delta
                .get("reasoning")
                .or_else(|| delta.get("reasoning_content"))
                .and_then(Value::as_str)
            {
                events.push(ModelEvent::ReasoningDelta {
                    text: reasoning.to_string(),
                });
            }
            if let Some(tool_calls) = delta.get("tool_calls").and_then(Value::as_array) {
                for tool_call in tool_calls {
                    let key = tool_call
                        .get("index")
                        .and_then(Value::as_u64)
                        .map(|index| index.to_string())
                        .or_else(|| {
                            tool_call
                                .get("id")
                                .and_then(Value::as_str)
                                .map(ToString::to_string)
                        })
                        .unwrap_or_else(|| "0".to_string());
                    events.push(ModelEvent::ToolCallStarted {
                        key: key.clone(),
                        id: tool_call
                            .get("id")
                            .and_then(Value::as_str)
                            .map(ToString::to_string),
                    });
                    if let Some(name) = tool_call
                        .get("function")
                        .and_then(|function| function.get("name"))
                        .and_then(Value::as_str)
                    {
                        events.push(ModelEvent::ToolCallNameDelta {
                            key: key.clone(),
                            text: name.to_string(),
                        });
                    }
                    if let Some(arguments) = tool_call
                        .get("function")
                        .and_then(|function| function.get("arguments"))
                        .and_then(Value::as_str)
                    {
                        events.push(ModelEvent::ToolCallArgumentsDelta {
                            key: key.clone(),
                            text: arguments.to_string(),
                        });
                    }
                }
            }
        }
        if let Some(finish_reason) = choice.get("finish_reason").and_then(Value::as_str) {
            events.push(ModelEvent::Completed {
                finish_reason: Some(finish_reason.to_string()),
            });
        }
    }
    events
}

fn decode_usage(usage: &Value) -> ModelUsage {
    ModelUsage {
        input_tokens: usage
            .get("prompt_tokens")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        output_tokens: usage
            .get("completion_tokens")
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        cache_read_input_tokens: usage
            .get("prompt_tokens_details")
            .and_then(|details| details.get("cached_tokens"))
            .and_then(Value::as_i64)
            .unwrap_or_default(),
        cache_creation_input_tokens: 0,
    }
}

fn take_sse_block(buffer: &str) -> Option<(String, String)> {
    if let Some(index) = buffer.find("\n\n") {
        return Some((
            buffer[..index].to_string(),
            buffer[index + 2..].to_string(),
        ));
    }
    if let Some(index) = buffer.find("\r\n\r\n") {
        return Some((
            buffer[..index].to_string(),
            buffer[index + 4..].to_string(),
        ));
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

    use super::{decode_openai_response_json, decode_openai_stream_frame};
    use crate::model::ModelEvent;

    #[test]
    fn decodes_non_streaming_tool_calls() {
        let events = decode_openai_response_json(&json!({
            "choices": [{
                "message": {
                    "tool_calls": [{
                        "id": "call-1",
                        "type": "function",
                        "function": {
                            "name": "echo",
                            "arguments": "{\"message\":\"hi\"}"
                        }
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {
                "prompt_tokens": 12,
                "completion_tokens": 3
            }
        }));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallArgumentsDelta { text, .. } if text == "{\"message\":\"hi\"}"
        )));
    }

    #[test]
    fn decodes_streaming_tool_call_fragments() {
        let events = decode_openai_stream_frame(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": {
                            "name": "read_file",
                            "arguments": "{\"path\":\"README.md\"}"
                        }
                    }]
                }
            }]
        }));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallNameDelta { text, .. } if text == "read_file"
        )));
    }

    #[test]
    fn streaming_tool_call_uses_index_for_followup_fragments() {
        let first = decode_openai_stream_frame(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "id": "call-1",
                        "function": { "name": "echo", "arguments": "" }
                    }]
                }
            }]
        }));
        let second = decode_openai_stream_frame(&json!({
            "choices": [{
                "delta": {
                    "tool_calls": [{
                        "index": 0,
                        "function": { "arguments": "{\"message\":\"hi\"}" }
                    }]
                }
            }]
        }));
        assert!(first.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallStarted { key, id } if key == "0" && id.as_deref() == Some("call-1")
        )));
        assert!(second.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallArgumentsDelta { key, text } if key == "0" && text == "{\"message\":\"hi\"}"
        )));
    }
}
