use crate::model::{ModelEvent, ModelMessage, ModelUsage, ReasoningMode};
use serde_json::Value;

pub(crate) fn serialize_chat_message(message: &ModelMessage) -> Value {
    let mut value = serde_json::to_value(message).expect("chat message should serialize");
    if let Some(object) = value.as_object_mut()
        && message.role == "assistant"
        && message.tool_calls.is_some()
        && !object.contains_key("content")
    {
        object.insert("content".to_string(), Value::Null);
    }
    value
}

pub(crate) fn decode_openai_response_json(
    response_body: &Value,
    reasoning_mode: &mut ReasoningMode,
) -> Vec<ModelEvent> {
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
            if content.contains("<think")
                || content.contains("<thinking")
                || content.contains("<thought")
            {
                *reasoning_mode = ReasoningMode::InlineTagged;
            }
            events.push(ModelEvent::TextSnapshot {
                text: content.to_string(),
            });
        }
        if let Some(reasoning) = choice
            .get("message")
            .and_then(|message| {
                message
                    .get("reasoning")
                    .or_else(|| message.get("reasoning_content"))
            })
            .and_then(Value::as_str)
        {
            *reasoning_mode = ReasoningMode::Structured;
            events.push(ModelEvent::ReasoningSnapshot {
                text: reasoning.to_string(),
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

pub(crate) fn decode_openai_stream_frame(
    frame: &Value,
    reasoning_mode: &mut ReasoningMode,
) -> Vec<ModelEvent> {
    let mut events = Vec::new();
    if let Some(error_message) = frame
        .get("error")
        .and_then(|error| error.get("message"))
        .and_then(Value::as_str)
    {
        events.push(ModelEvent::Failed {
            message: error_message.to_string(),
        });
    }
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
                if *reasoning_mode != ReasoningMode::Structured
                    && (content.contains("<think")
                        || content.contains("<thinking")
                        || content.contains("<thought"))
                {
                    *reasoning_mode = ReasoningMode::InlineTagged;
                }
                events.push(ModelEvent::TextDelta {
                    text: content.to_string(),
                });
            }
            if let Some(reasoning) = delta
                .get("reasoning")
                .or_else(|| delta.get("reasoning_content"))
                .and_then(Value::as_str)
            {
                *reasoning_mode = ReasoningMode::Structured;
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
