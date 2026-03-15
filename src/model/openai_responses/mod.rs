use std::time::Duration;
use async_trait::async_trait;
use chrono::Utc;
use futures_util::StreamExt;
use reqwest::StatusCode;
use reqwest::header::CONTENT_TYPE;
use serde_json::{Value, json};

use crate::model::openai_compat::OpenAiCompatibleConfig;
use crate::model::{
    AccumulationMode, ExchangeAccumulator, ModelEngineError, ModelEvent, ModelExchangeRequest,
    ModelExchangeResult, ModelRunner, ModelUsage, RawFrame, RawModelTrace,
    ReasoningMode, TraceOutcome, TransportKind,
};

pub(crate) mod decode;
pub(crate) mod payload;
#[cfg(test)]
mod tests;

use decode::*;
use payload::*;

#[derive(Debug)]
pub struct OpenAiResponsesEngine {
    client: reqwest::Client,
    config: OpenAiCompatibleConfig,
}

impl OpenAiResponsesEngine {
    const TERMINAL_FRAME_GRACE: Duration = Duration::from_secs(1);

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
        if let Some(extra) = request.extra.as_object()
            && let Some(target) = payload.as_object_mut() {
                for (key, value) in extra {
                    target.insert(key.clone(), value.clone());
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
        let mut accumulator =
            self.new_accumulator(&request.model, AccumulationMode::DeltaPlusFinal);
        let mut events = vec![ModelEvent::ExchangeStarted];
        accumulator.push(&events[0]);

        let mut stream = response.bytes_stream();
        let mut buffer = String::new();
        let mut frame_index = 0usize;
        let mut saw_done = false;
        let mut saw_terminal_event = false;
        loop {
            let next_chunk = if saw_terminal_event {
                match tokio::time::timeout(Self::TERMINAL_FRAME_GRACE, stream.next()).await {
                    Ok(chunk) => chunk,
                    Err(_) => break,
                }
            } else {
                stream.next().await
            };
            let Some(chunk) = next_chunk else {
                break;
            };
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
                let frame_events = decode_responses_stream_frame(&frame_value, &mut reasoning_mode);
                if frame_events.iter().any(|event| {
                    matches!(
                        event,
                        ModelEvent::Completed { .. } | ModelEvent::Failed { .. }
                    )
                }) {
                    saw_terminal_event = true;
                }
                for event in frame_events {
                    accumulator.push(&event);
                    events.push(event);
                }
            }
        }

        if !buffer.trim().is_empty()
            && let Ok(frame_value) = serde_json::from_str::<Value>(buffer.trim()) {
                raw_trace.raw_frames.push(RawFrame {
                    sequence: frame_index,
                    data: frame_value.clone(),
                });
                let frame_events = decode_responses_stream_frame(&frame_value, &mut reasoning_mode);
                if frame_events.iter().any(|event| {
                    matches!(
                        event,
                        ModelEvent::Completed { .. } | ModelEvent::Failed { .. }
                    )
                }) {
                    saw_terminal_event = true;
                }
                for event in frame_events {
                    accumulator.push(&event);
                    events.push(event);
                }
            }

        if !saw_done
            && !saw_terminal_event
            && !events
                .iter()
                .any(|event| matches!(event, ModelEvent::Completed { .. }))
        {
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
        let retry_after = OpenAiCompatibleConfig::retry_after(response.headers());
        if response.status() != StatusCode::OK {
            let status_code = response.status();
            let status = response.status().as_u16();
            let body_text = response.text().await.unwrap_or_default();
            let body_json =
                serde_json::from_str(&body_text).unwrap_or_else(|_| json!({ "body": body_text }));
            if let Some(rate_limit_message) =
                OpenAiCompatibleConfig::rate_limit_message(Some(status_code), &body_json)
            {
                let message = format!(
                    "{} rate limited: {}",
                    self.config.provider_name, rate_limit_message
                );
                let exchange = self.reduced_error_result(
                    &request,
                    started_at,
                    payload,
                    provider_request_id,
                    TransportKind::HttpJson,
                    message.clone(),
                    Some(body_json),
                    AccumulationMode::FullSnapshot,
                    ReasoningMode::Unknown,
                );
                return Err(ModelEngineError::RateLimited {
                    message,
                    retry_after,
                    exchange: Box::new(exchange),
                });
            }
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
            let exchange = self
                .decode_sse_response(&request, started_at, payload, provider_request_id, response)
                .await?;
            if let Some(rate_limit_message) = exchange
                .raw_trace
                .raw_frames
                .iter()
                .find_map(|frame| OpenAiCompatibleConfig::rate_limit_message(None, &frame.data))
                .or_else(|| {
                    exchange
                        .error_summary
                        .as_deref()
                        .filter(|message| {
                            OpenAiCompatibleConfig::looks_like_rate_limit_text(message)
                        })
                        .map(ToString::to_string)
                })
            {
                return Err(ModelEngineError::RateLimited {
                    message: format!(
                        "{} rate limited: {}",
                        self.config.provider_name, rate_limit_message
                    ),
                    retry_after,
                    exchange: Box::new(exchange),
                });
            }
            Ok(exchange)
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
            if let Some(rate_limit_message) =
                OpenAiCompatibleConfig::rate_limit_message(None, &response_body)
            {
                let message = format!(
                    "{} rate limited: {}",
                    self.config.provider_name, rate_limit_message
                );
                let exchange = self.reduced_error_result(
                    &request,
                    started_at,
                    payload,
                    provider_request_id,
                    TransportKind::HttpJson,
                    message.clone(),
                    Some(response_body),
                    AccumulationMode::FullSnapshot,
                    ReasoningMode::Unknown,
                );
                return Err(ModelEngineError::RateLimited {
                    message,
                    retry_after,
                    exchange: Box::new(exchange),
                });
            }
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
