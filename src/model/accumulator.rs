use std::collections::HashMap;

use serde_json::json;

use crate::model::{
    ModelEvent, ModelExchangeResult, ModelUsage, RawModelTrace, ReducedToolCall, TraceOutcome,
};

#[derive(Debug, Clone)]
struct PartialToolCall {
    id: Option<String>,
    name: String,
    arguments_text: String,
    finished: bool,
}

pub struct ExchangeAccumulator {
    model: String,
    text: String,
    reasoning: String,
    usage: ModelUsage,
    finish_reason: Option<String>,
    tool_order: Vec<String>,
    tool_calls: HashMap<String, PartialToolCall>,
    errors: Vec<String>,
}

impl ExchangeAccumulator {
    pub fn new(model: impl Into<String>) -> Self {
        Self {
            model: model.into(),
            text: String::new(),
            reasoning: String::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            tool_order: Vec::new(),
            tool_calls: HashMap::new(),
            errors: Vec::new(),
        }
    }

    pub fn push(&mut self, event: &ModelEvent) {
        match event {
            ModelEvent::ExchangeStarted => {}
            ModelEvent::TextDelta { text } => self.text.push_str(text),
            ModelEvent::ReasoningDelta { text } => self.reasoning.push_str(text),
            ModelEvent::ToolCallStarted { key, id } => {
                self.ensure_tool_call(key, id.clone());
            }
            ModelEvent::ToolCallNameDelta { key, text } => {
                self.ensure_tool_call(key, None).name.push_str(text);
            }
            ModelEvent::ToolCallArgumentsDelta { key, text } => {
                self.ensure_tool_call(key, None).arguments_text.push_str(text);
            }
            ModelEvent::ToolCallFinished { key } => {
                self.ensure_tool_call(key, None).finished = true;
            }
            ModelEvent::UsageUpdated { usage } => self.usage = usage.clone(),
            ModelEvent::Completed { finish_reason } => {
                self.finish_reason = finish_reason.clone();
            }
            ModelEvent::Failed { message } => self.errors.push(message.clone()),
        }
    }

    fn ensure_tool_call(&mut self, key: &str, id: Option<String>) -> &mut PartialToolCall {
        if !self.tool_calls.contains_key(key) {
            self.tool_order.push(key.to_string());
            self.tool_calls.insert(
                key.to_string(),
                PartialToolCall {
                    id,
                    name: String::new(),
                    arguments_text: String::new(),
                    finished: false,
                },
            );
        } else if let Some(id) = id {
            if let Some(call) = self.tool_calls.get_mut(key) {
                call.id = Some(id);
            }
        }
        self.tool_calls.get_mut(key).expect("tool call should exist")
    }

    pub fn build(
        mut self,
        started_at: chrono::DateTime<chrono::Utc>,
        completed_at: chrono::DateTime<chrono::Utc>,
        raw_trace: RawModelTrace,
        normalized_events: Vec<ModelEvent>,
    ) -> ModelExchangeResult {
        let mut reduced_tool_calls = Vec::new();
        for key in &self.tool_order {
            let Some(partial) = self.tool_calls.get(key) else {
                continue;
            };
            if partial.name.trim().is_empty() {
                self.errors
                    .push(format!("tool call '{key}' completed without a function name"));
                continue;
            }
            let arguments_json = if partial.arguments_text.trim().is_empty() {
                Some(json!({}))
            } else {
                match serde_json::from_str(&partial.arguments_text) {
                    Ok(value) => Some(value),
                    Err(error) => {
                        self.errors.push(format!(
                            "tool call '{}' has malformed JSON arguments: {}",
                            partial.name, error
                        ));
                        None
                    }
                }
            };
            reduced_tool_calls.push(ReducedToolCall {
                id: partial
                    .id
                    .clone()
                    .unwrap_or_else(|| format!("tool-call-{}", reduced_tool_calls.len())),
                name: partial.name.clone(),
                arguments_json,
                arguments_text: partial.arguments_text.clone(),
            });
        }

        ModelExchangeResult {
            model: self.model,
            request_started_at: started_at,
            request_completed_at: completed_at,
            content: (!self.text.is_empty()).then_some(self.text),
            reasoning: (!self.reasoning.is_empty()).then_some(self.reasoning),
            tool_calls: reduced_tool_calls,
            usage: self.usage,
            finish_reason: self.finish_reason,
            raw_trace,
            normalized_events,
            outcome: if self.errors.is_empty() {
                TraceOutcome::Ok
            } else {
                TraceOutcome::ParseError
            },
            error_summary: (!self.errors.is_empty()).then(|| self.errors.join("; ")),
        }
    }
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::ExchangeAccumulator;
    use crate::model::{ModelEvent, RawModelTrace, TransportKind, TraceOutcome};

    #[test]
    fn assembles_fragmented_tool_calls() {
        let mut accumulator = ExchangeAccumulator::new("test-model");
        let events = vec![
            ModelEvent::ExchangeStarted,
            ModelEvent::ToolCallStarted {
                key: "0".to_string(),
                id: Some("call-1".to_string()),
            },
            ModelEvent::ToolCallNameDelta {
                key: "0".to_string(),
                text: "read_file".to_string(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "0".to_string(),
                text: "{\"path\":\"".to_string(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "0".to_string(),
                text: "README.md\"}".to_string(),
            },
            ModelEvent::ToolCallFinished {
                key: "0".to_string(),
            },
            ModelEvent::Completed {
                finish_reason: Some("tool_calls".to_string()),
            },
        ];
        for event in &events {
            accumulator.push(event);
        }

        let result = accumulator.build(
            Utc::now(),
            Utc::now(),
            RawModelTrace {
                request_body: json!({}),
                response_body: None,
                raw_frames: Vec::new(),
                provider_request_id: None,
                transport_kind: TransportKind::HttpSse,
            },
            events,
        );

        assert_eq!(result.outcome, TraceOutcome::Ok);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(result.tool_calls[0].arguments_json, Some(json!({"path":"README.md"})));
    }

    #[test]
    fn preserves_invalid_json_without_fallback() {
        let mut accumulator = ExchangeAccumulator::new("test-model");
        let events = vec![
            ModelEvent::ToolCallStarted {
                key: "0".to_string(),
                id: Some("call-1".to_string()),
            },
            ModelEvent::ToolCallNameDelta {
                key: "0".to_string(),
                text: "echo".to_string(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "0".to_string(),
                text: "{".to_string(),
            },
            ModelEvent::ToolCallFinished {
                key: "0".to_string(),
            },
            ModelEvent::Completed { finish_reason: None },
        ];
        for event in &events {
            accumulator.push(event);
        }
        let result = accumulator.build(
            Utc::now(),
            Utc::now(),
            RawModelTrace {
                request_body: json!({}),
                response_body: None,
                raw_frames: Vec::new(),
                provider_request_id: None,
                transport_kind: TransportKind::HttpJson,
            },
            events,
        );
        assert_eq!(result.outcome, TraceOutcome::ParseError);
        assert_eq!(result.tool_calls[0].arguments_json, None);
        assert_eq!(result.tool_calls[0].arguments_text, "{");
    }
}
