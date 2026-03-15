use crate::model::{ModelEvent, ModelUsage, ReasoningMode};
use serde_json::Value;
use std::collections::HashMap;

pub(crate) fn decode_responses_json(
    response_body: &Value,
    reasoning_mode: &mut ReasoningMode,
) -> Vec<ModelEvent> {
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

pub(crate) fn decode_responses_stream_frame(
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

    let kind = frame
        .get("type")
        .and_then(Value::as_str)
        .unwrap_or_default();
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
                .or_else(|| {
                    item.get("call_id")
                        .and_then(Value::as_str)
                        .map(ToString::to_string)
                })
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
                if text.contains("<think")
                    || text.contains("<thinking")
                    || text.contains("<thought")
                {
                    *reasoning_mode = ReasoningMode::InlineTagged;
                }
                let previous = state
                    .text_snapshots
                    .insert(message_id.clone(), text.clone());
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

pub(crate) fn take_sse_block(buffer: &str) -> Option<(String, String)> {
    if let Some(index) = buffer.find("\n\n") {
        return Some((buffer[..index].to_string(), buffer[index + 2..].to_string()));
    }
    if let Some(index) = buffer.find("\r\n\r\n") {
        return Some((buffer[..index].to_string(), buffer[index + 4..].to_string()));
    }
    None
}

pub(crate) fn parse_sse_data(block: &str) -> Option<String> {
    let mut lines = Vec::new();
    for line in block.lines() {
        if let Some(rest) = line.strip_prefix("data:") {
            lines.push(rest.trim_start().to_string());
        }
    }
    (!lines.is_empty()).then(|| lines.join("\n"))
}
