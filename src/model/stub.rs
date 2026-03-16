use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::model::{
    AccumulationMode, ExchangeAccumulator, ModelEngineError, ModelEvent, ModelExchangeRequest,
    ModelExchangeResult, ModelRunner, RawModelTrace, ReasoningMode, TraceOutcome, TransportKind,
};

#[derive(Debug, Default)]
pub struct StubModelEngine {
    attempts: Mutex<HashMap<String, usize>>,
}

#[async_trait]
impl ModelRunner for StubModelEngine {
    async fn run(
        &self,
        request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, ModelEngineError> {
        let started_at = Utc::now();
        let last_message = request
            .messages
            .last()
            .and_then(|message| message.content.clone())
            .unwrap_or_default();
        let tool_messages = request
            .messages
            .iter()
            .filter(|message| message.role == "tool")
            .filter_map(|message| message.content.clone())
            .collect::<Vec<_>>();
        let raw_request = json!({
            "model": request.model,
            "messages": request.messages,
            "tools": request.tools,
            "stream": request.stream,
            "max_tokens": request.max_tokens,
            "response_format": request.response_format,
            "extra": request.extra,
        });

        let mut events = vec![ModelEvent::ExchangeStarted];
        let mut accumulator =
            ExchangeAccumulator::new(request.model.clone(), AccumulationMode::FullSnapshot);
        accumulator.push(&events[0]);

        let mut response_body = json!({});
        if request.extra.get("betterclaw_role").and_then(Value::as_str) == Some("compressor") {
            let text = json!({
                "wake_pack": "Stub compressor wake pack: preserve recent user intent and tool outcomes.",
                "invariant_self": [
                    {
                        "text": "BetterClaw should prefer tool-backed answers when tools materially help.",
                        "citations": ["turn:stub:user"]
                    }
                ],
                "invariant_user": [
                    {
                        "text": "The user is actively iterating on BetterClaw runtime behavior.",
                        "citations": ["turn:stub:user"]
                    }
                ],
                "invariant_relationship": [],
                "drift_flags": [],
                "drift_contradictions": [],
                "drift_merges": [],
                "summary": "Stub compressor distill complete."
            })
            .to_string();
            for event in [
                ModelEvent::TextSnapshot { text },
                ModelEvent::Completed {
                    finish_reason: Some("stop".to_string()),
                },
            ] {
                accumulator.push(&event);
                events.push(event);
            }
        } else if let Some(rest) = last_message.strip_prefix("/rate-limit-once ") {
            let retry_after = rest.trim().parse::<u64>().unwrap_or(1);
            let mut attempts = self.attempts.lock().expect("stub attempts lock");
            let seen = attempts.entry(last_message.clone()).or_insert(0);
            *seen += 1;
            if *seen == 1 {
                let message = format!("simulated rate limit for {retry_after}ms");
                response_body = json!({
                    "error": {
                        "code": "rate_limit_exceeded",
                        "message": message,
                    }
                });
                let mut result = accumulator.build(
                    started_at,
                    Utc::now(),
                    RawModelTrace {
                        request_body: raw_request,
                        response_body: Some(response_body),
                        raw_frames: Vec::new(),
                        provider_request_id: Some(Uuid::new_v4().to_string()),
                        transport_kind: TransportKind::HttpJson,
                        accumulation_mode: AccumulationMode::FullSnapshot,
                        reasoning_mode: ReasoningMode::Unknown,
                    },
                    events,
                );
                result.outcome = TraceOutcome::TransportError;
                result.error_summary = Some(message.clone());
                return Err(ModelEngineError::RateLimited {
                    message,
                    retry_after: Some(Duration::from_millis(retry_after)),
                    exchange: Box::new(result),
                });
            }
        } else if last_message.starts_with("/rate-limit-backoff-once") {
            let mut attempts = self.attempts.lock().expect("stub attempts lock");
            let seen = attempts.entry(last_message.clone()).or_insert(0);
            *seen += 1;
            if *seen == 1 {
                let message = "simulated rate limit without retry-after".to_string();
                response_body = json!({
                    "error": {
                        "code": "rate_limit_exceeded",
                        "message": message,
                    }
                });
                let mut result = accumulator.build(
                    started_at,
                    Utc::now(),
                    RawModelTrace {
                        request_body: raw_request,
                        response_body: Some(response_body),
                        raw_frames: Vec::new(),
                        provider_request_id: Some(Uuid::new_v4().to_string()),
                        transport_kind: TransportKind::HttpJson,
                        accumulation_mode: AccumulationMode::FullSnapshot,
                        reasoning_mode: ReasoningMode::Unknown,
                    },
                    events,
                );
                result.outcome = TraceOutcome::TransportError;
                result.error_summary = Some(message.clone());
                return Err(ModelEngineError::RateLimited {
                    message,
                    retry_after: None,
                    exchange: Box::new(result),
                });
            }
        }

        if let Some(rest) = last_message.strip_prefix("/tool-batch ") {
            let parsed_calls: Vec<Value> = serde_json::from_str(rest).unwrap_or_default();
            let mut tool_events = Vec::new();
            let mut tool_call_payloads = Vec::new();
            for (index, call) in parsed_calls.iter().enumerate() {
                let tool_name = call
                    .get("name")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string();
                let args = call
                    .get("arguments")
                    .cloned()
                    .unwrap_or_else(|| json!({}))
                    .to_string();
                let key = index.to_string();
                let tool_id = Uuid::new_v4().to_string();
                tool_events.extend([
                    ModelEvent::ToolCallStarted {
                        key: key.clone(),
                        id: Some(tool_id.clone()),
                    },
                    ModelEvent::ToolCallNameDelta {
                        key: key.clone(),
                        text: tool_name.clone(),
                    },
                    ModelEvent::ToolCallArgumentsDelta {
                        key: key.clone(),
                        text: args.clone(),
                    },
                    ModelEvent::ToolCallFinished { key },
                ]);
                tool_call_payloads.push(json!({
                    "id": tool_id,
                    "type": "function",
                    "function": {
                        "name": tool_name,
                        "arguments": args,
                    }
                }));
            }
            tool_events.push(ModelEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
            });
            response_body = json!({
                "choices": [{
                    "message": {
                        "tool_calls": tool_call_payloads
                    },
                    "finish_reason": "tool_calls"
                }]
            });
            for event in tool_events {
                accumulator.push(&event);
                events.push(event);
            }
        } else if let Some(rest) = last_message.strip_prefix("/final-message ") {
            let key = "0".to_string();
            let tool_id = Uuid::new_v4().to_string();
            let args = json!({ "content": rest }).to_string();
            let tool_events = vec![
                ModelEvent::ToolCallStarted {
                    key: key.clone(),
                    id: Some(tool_id.clone()),
                },
                ModelEvent::ToolCallNameDelta {
                    key: key.clone(),
                    text: "final_message".to_string(),
                },
                ModelEvent::ToolCallArgumentsDelta {
                    key: key.clone(),
                    text: args.clone(),
                },
                ModelEvent::ToolCallFinished { key },
                ModelEvent::Completed {
                    finish_reason: Some("tool_calls".to_string()),
                },
            ];
            response_body = json!({
                "choices": [{
                    "message": {
                        "tool_calls": [{
                            "id": tool_id,
                            "type": "function",
                            "function": {
                                "name": "final_message",
                                "arguments": args,
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            });
            for event in tool_events {
                accumulator.push(&event);
                events.push(event);
            }
        } else if let Some(rest) = last_message.strip_prefix("/tool ") {
            let mut parts = rest.splitn(2, ' ');
            let tool_name = parts.next().unwrap_or_default();
            let args = parts.next().unwrap_or("{}");
            let key = "0".to_string();
            let tool_id = Uuid::new_v4().to_string();
            let tool_events = vec![
                ModelEvent::ToolCallStarted {
                    key: key.clone(),
                    id: Some(tool_id.clone()),
                },
                ModelEvent::ToolCallNameDelta {
                    key: key.clone(),
                    text: tool_name.to_string(),
                },
                ModelEvent::ToolCallArgumentsDelta {
                    key: key.clone(),
                    text: args.to_string(),
                },
                ModelEvent::ToolCallFinished { key },
                ModelEvent::Completed {
                    finish_reason: Some("tool_calls".to_string()),
                },
            ];
            response_body = json!({
                "choices": [{
                    "message": {
                        "tool_calls": [{
                            "id": tool_id,
                            "type": "function",
                            "function": {
                                "name": tool_name,
                                "arguments": args,
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }]
            });
            for event in tool_events {
                accumulator.push(&event);
                events.push(event);
            }
        } else {
            let tool_hint = if request.tools.is_empty() {
                String::new()
            } else {
                format!(" ({} tools available)", request.tools.len())
            };
            let text = if tool_messages.is_empty() {
                format!("Echo: {last_message}{tool_hint}")
            } else {
                format!("Echo: {}{tool_hint}", tool_messages.join("\n"))
            };
            response_body = json!({
                "choices": [{
                    "message": { "content": text },
                    "finish_reason": "stop"
                }]
            });
            for event in [
                ModelEvent::TextSnapshot { text },
                ModelEvent::Completed {
                    finish_reason: Some("stop".to_string()),
                },
            ] {
                accumulator.push(&event);
                events.push(event);
            }
        }

        let completed_at = Utc::now();
        let mut result = accumulator.build(
            started_at,
            completed_at,
            RawModelTrace {
                request_body: raw_request,
                response_body: Some(response_body),
                raw_frames: Vec::new(),
                provider_request_id: Some(Uuid::new_v4().to_string()),
                transport_kind: TransportKind::HttpJson,
                accumulation_mode: AccumulationMode::FullSnapshot,
                reasoning_mode: ReasoningMode::Unknown,
            },
            events,
        );
        if last_message.starts_with("/transport-error ") {
            result.outcome = TraceOutcome::TransportError;
            result.error_summary = Some("simulated transport failure".to_string());
            return Err(ModelEngineError::TransportFailure {
                message: "simulated transport failure".to_string(),
                exchange: Box::new(result),
            });
        }
        Ok(result)
    }
}
