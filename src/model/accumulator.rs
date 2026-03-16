use std::collections::HashMap;

use serde_json::json;

use crate::model::{
    AccumulationMode, ModelEvent, ModelExchangeResult, ModelUsage, RawModelTrace, ReducedToolCall,
    TraceOutcome, split_inline_reasoning,
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
    accumulation_mode: AccumulationMode,
    text: String,
    reasoning: String,
    usage: ModelUsage,
    finish_reason: Option<String>,
    tool_order: Vec<String>,
    tool_aliases: HashMap<String, String>,
    tool_calls: HashMap<String, PartialToolCall>,
    errors: Vec<String>,
}

impl ExchangeAccumulator {
    pub fn new(model: impl Into<String>, accumulation_mode: AccumulationMode) -> Self {
        Self {
            model: model.into(),
            accumulation_mode,
            text: String::new(),
            reasoning: String::new(),
            usage: ModelUsage::default(),
            finish_reason: None,
            tool_order: Vec::new(),
            tool_aliases: HashMap::new(),
            tool_calls: HashMap::new(),
            errors: Vec::new(),
        }
    }

    pub fn push(&mut self, event: &ModelEvent) {
        match event {
            ModelEvent::ExchangeStarted => {}
            ModelEvent::TextDelta { text } => self.text.push_str(text),
            ModelEvent::TextSnapshot { text } => self.text = text.clone(),
            ModelEvent::TextFinal { text } => self.text = text.clone(),
            ModelEvent::ReasoningDelta { text } => self.reasoning.push_str(text),
            ModelEvent::ReasoningSnapshot { text } => self.reasoning = text.clone(),
            ModelEvent::ReasoningFinal { text } => self.reasoning = text.clone(),
            ModelEvent::ToolCallStarted { key, id } => {
                self.ensure_tool_call(key, id.clone());
            }
            ModelEvent::ToolCallNameDelta { key, text } => {
                merge_tool_call_text(&mut self.ensure_tool_call(key, None).name, text);
            }
            ModelEvent::ToolCallArgumentsDelta { key, text } => {
                merge_tool_call_text(
                    &mut self.ensure_tool_call(key, None).arguments_text,
                    text,
                );
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
        let canonical_key = self
            .tool_aliases
            .get(key)
            .cloned()
            .unwrap_or_else(|| key.to_string());

        if !self.tool_calls.contains_key(&canonical_key)
            && let Some(id_value) = id.as_deref()
            && let Some(existing_key) = self.tool_calls.iter().find_map(|(existing_key, call)| {
                (call.id.as_deref() == Some(id_value)).then(|| existing_key.clone())
            })
        {
            self.tool_aliases.insert(canonical_key.clone(), existing_key);
        }

        let canonical_key = self
            .tool_aliases
            .get(&canonical_key)
            .cloned()
            .unwrap_or(canonical_key);

        if !self.tool_calls.contains_key(&canonical_key) {
            self.tool_order.push(canonical_key.clone());
            self.tool_calls.insert(
                canonical_key.clone(),
                PartialToolCall {
                    id,
                    name: String::new(),
                    arguments_text: String::new(),
                    finished: false,
                },
            );
        } else if let Some(id) = id
            && let Some(call) = self.tool_calls.get_mut(&canonical_key)
        {
            call.id = Some(id);
        }
        self.tool_calls
            .get_mut(&canonical_key)
            .expect("tool call should exist")
    }

    pub fn build(
        mut self,
        started_at: chrono::DateTime<chrono::Utc>,
        completed_at: chrono::DateTime<chrono::Utc>,
        raw_trace: RawModelTrace,
        normalized_events: Vec<ModelEvent>,
    ) -> ModelExchangeResult {
        if self.accumulation_mode == AccumulationMode::DeltaPlusFinal {
            // No-op today other than making the mode explicit in traces; final overwrite
            // behavior is driven by TextFinal/ReasoningFinal events during accumulation.
        }
        let mut reduced_tool_calls = Vec::new();
        for key in &self.tool_order {
            let Some(partial) = self.tool_calls.get(key) else {
                continue;
            };
            if partial.name.trim().is_empty() {
                self.errors.push(format!(
                    "tool call '{key}' completed without a function name"
                ));
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

        let mut content = (!self.text.is_empty()).then_some(self.text);
        let mut reasoning = (!self.reasoning.is_empty()).then_some(self.reasoning);
        if let Some(text) = content.take() {
            let (inline_reasoning, sanitized_content) = split_inline_reasoning(&text);
            if let Some(inline_reasoning) = inline_reasoning {
                reasoning = Some(match reasoning.take() {
                    Some(existing) if !existing.trim().is_empty() => {
                        format!("{}\n{}", existing.trim(), inline_reasoning)
                    }
                    _ => inline_reasoning,
                });
            }
            content = (!sanitized_content.is_empty()).then_some(sanitized_content);
        }

        ModelExchangeResult {
            model: self.model,
            request_started_at: started_at,
            request_completed_at: completed_at,
            content,
            reasoning,
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

fn merge_tool_call_text(current: &mut String, incoming: &str) {
    if incoming.is_empty() {
        return;
    }
    if current.is_empty() {
        current.push_str(incoming);
        return;
    }
    if current == incoming {
        return;
    }
    if incoming.starts_with(current.as_str()) {
        current.clear();
        current.push_str(incoming);
        return;
    }
    current.push_str(incoming);
}

#[cfg(test)]
mod tests {
    use chrono::Utc;
    use serde_json::json;

    use super::ExchangeAccumulator;
    use crate::model::{AccumulationMode, ModelEvent, RawModelTrace, TraceOutcome, TransportKind};

    #[test]
    fn assembles_fragmented_tool_calls() {
        let mut accumulator = ExchangeAccumulator::new("test-model", AccumulationMode::Delta);
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
                accumulation_mode: AccumulationMode::Delta,
                reasoning_mode: crate::model::ReasoningMode::Unknown,
            },
            events,
        );

        assert_eq!(result.outcome, TraceOutcome::Ok);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(
            result.tool_calls[0].arguments_json,
            Some(json!({"path":"README.md"}))
        );
    }

    #[test]
    fn preserves_invalid_json_without_fallback() {
        let mut accumulator = ExchangeAccumulator::new("test-model", AccumulationMode::Delta);
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
            ModelEvent::Completed {
                finish_reason: None,
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
                transport_kind: TransportKind::HttpJson,
                accumulation_mode: AccumulationMode::Delta,
                reasoning_mode: crate::model::ReasoningMode::Unknown,
            },
            events,
        );
        assert_eq!(result.outcome, TraceOutcome::ParseError);
        assert_eq!(result.tool_calls[0].arguments_json, None);
        assert_eq!(result.tool_calls[0].arguments_text, "{");
    }

    #[test]
    fn promotes_inline_reasoning_tags_to_reasoning_field() {
        let mut accumulator = ExchangeAccumulator::new("test-model", AccumulationMode::Delta);
        let events = vec![
            ModelEvent::TextDelta {
                text: "<think>hidden</think>Visible".to_string(),
            },
            ModelEvent::Completed {
                finish_reason: None,
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
                transport_kind: TransportKind::HttpJson,
                accumulation_mode: AccumulationMode::Delta,
                reasoning_mode: crate::model::ReasoningMode::Unknown,
            },
            events,
        );
        assert_eq!(result.content.as_deref(), Some("Visible"));
        assert_eq!(result.reasoning.as_deref(), Some("hidden"));
    }

    #[test]
    fn snapshot_events_replace_prior_text() {
        let mut accumulator =
            ExchangeAccumulator::new("test-model", AccumulationMode::FullSnapshot);
        let events = vec![
            ModelEvent::TextSnapshot {
                text: "Hel".to_string(),
            },
            ModelEvent::TextSnapshot {
                text: "Hello".to_string(),
            },
            ModelEvent::Completed {
                finish_reason: None,
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
                accumulation_mode: AccumulationMode::FullSnapshot,
                reasoning_mode: crate::model::ReasoningMode::Unknown,
            },
            events,
        );
        assert_eq!(result.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn final_events_override_prior_delta_text() {
        let mut accumulator =
            ExchangeAccumulator::new("test-model", AccumulationMode::DeltaPlusFinal);
        let events = vec![
            ModelEvent::TextDelta {
                text: "He".to_string(),
            },
            ModelEvent::TextDelta {
                text: "llo??".to_string(),
            },
            ModelEvent::TextFinal {
                text: "Hello".to_string(),
            },
            ModelEvent::Completed {
                finish_reason: None,
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
                transport_kind: TransportKind::SessionStream,
                accumulation_mode: AccumulationMode::DeltaPlusFinal,
                reasoning_mode: crate::model::ReasoningMode::Unknown,
            },
            events,
        );
        assert_eq!(result.content.as_deref(), Some("Hello"));
    }

    #[test]
    fn merges_tool_call_aliases_by_call_id() {
        let mut accumulator = ExchangeAccumulator::new("test-model", AccumulationMode::Delta);
        let events = vec![
            ModelEvent::ToolCallStarted {
                key: "output_index:1".to_string(),
                id: None,
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "output_index:1".to_string(),
                text: "{\"path\":\"README.md\"}".to_string(),
            },
            ModelEvent::ToolCallStarted {
                key: "output_index:1".to_string(),
                id: Some("call-1".to_string()),
            },
            ModelEvent::ToolCallNameDelta {
                key: "output_index:1".to_string(),
                text: "read_file".to_string(),
            },
            ModelEvent::ToolCallStarted {
                key: "call-1".to_string(),
                id: Some("call-1".to_string()),
            },
            ModelEvent::ToolCallNameDelta {
                key: "call-1".to_string(),
                text: "read_file".to_string(),
            },
            ModelEvent::ToolCallStarted {
                key: "other-key".to_string(),
                id: Some("call-1".to_string()),
            },
            ModelEvent::ToolCallFinished {
                key: "other-key".to_string(),
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
                accumulation_mode: AccumulationMode::Delta,
                reasoning_mode: crate::model::ReasoningMode::Unknown,
            },
            events,
        );

        assert_eq!(result.outcome, TraceOutcome::Ok);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(result.tool_calls[0].id, "call-1");
        assert_eq!(result.tool_calls[0].name, "read_file");
        assert_eq!(
            result.tool_calls[0].arguments_json,
            Some(json!({"path":"README.md"}))
        );
    }

    #[test]
    fn preserves_suffix_like_argument_deltas_before_final_snapshot() {
        let mut accumulator = ExchangeAccumulator::new("test-model", AccumulationMode::Delta);
        let events = vec![
            ModelEvent::ToolCallStarted {
                key: "output_index:1".to_string(),
                id: Some("call-1".to_string()),
            },
            ModelEvent::ToolCallNameDelta {
                key: "output_index:1".to_string(),
                text: "read_file".to_string(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "output_index:1".to_string(),
                text: "{\"limit\":".to_string(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "output_index:1".to_string(),
                text: "100".to_string(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "output_index:1".to_string(),
                text: "00".to_string(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "output_index:1".to_string(),
                text: ",\"offset\":1}".to_string(),
            },
            ModelEvent::ToolCallArgumentsDelta {
                key: "output_index:1".to_string(),
                text: "{\"limit\":10000,\"offset\":1}".to_string(),
            },
            ModelEvent::ToolCallFinished {
                key: "output_index:1".to_string(),
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
                accumulation_mode: AccumulationMode::Delta,
                reasoning_mode: crate::model::ReasoningMode::Unknown,
            },
            events,
        );

        assert_eq!(result.outcome, TraceOutcome::Ok);
        assert_eq!(result.tool_calls.len(), 1);
        assert_eq!(
            result.tool_calls[0].arguments_text,
            "{\"limit\":10000,\"offset\":1}"
        );
        assert_eq!(
            result.tool_calls[0].arguments_json,
            Some(json!({"limit": 10000, "offset": 1}))
        );
    }
}
