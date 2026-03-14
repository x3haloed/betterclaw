use async_trait::async_trait;
use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::model::{
    ExchangeAccumulator, ModelEngineError, ModelEvent, ModelExchangeRequest, ModelExchangeResult,
    ModelRunner, RawModelTrace, TraceOutcome, TransportKind,
};

#[derive(Debug, Default)]
pub struct StubModelEngine;

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
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let raw_request = json!({
            "model": request.model,
            "messages": request.messages,
            "tools": request.tools,
            "stream": request.stream,
        });

        let mut events = vec![ModelEvent::ExchangeStarted];
        let mut accumulator = ExchangeAccumulator::new(request.model.clone());
        accumulator.push(&events[0]);

        let mut response_body = json!({});
        if let Some(rest) = last_message.strip_prefix("/tool ") {
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
            let text = format!("Echo: {last_message}{tool_hint}");
            response_body = json!({
                "choices": [{
                    "message": { "content": text },
                    "finish_reason": "stop"
                }]
            });
            for event in [
                ModelEvent::TextDelta { text },
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
