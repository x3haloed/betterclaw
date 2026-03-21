use crate::model::{ContentPart, MessageContent, ModelMessage};
use serde::Serialize;
use serde_json::{Value, json};

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
#[serde(tag = "type", rename_all = "snake_case")]
enum ResponseContentItem {
    InputText { text: String },
    OutputText { text: String },
    ImageUrl { image_url: ResponseImageUrl },
}

#[derive(Debug, Serialize)]
struct ResponseImageUrl {
    url: String,
}

fn response_text_item(role: &str, text: String) -> ResponseContentItem {
    match role {
        "assistant" => ResponseContentItem::OutputText { text },
        _ => ResponseContentItem::InputText { text },
    }
}

/// Convert a MessageContent into ResponseContentItems for the Responses API.
fn content_to_response_items(role: &str, content: &MessageContent) -> Vec<ResponseContentItem> {
    match content {
        MessageContent::Text(text) if !text.is_empty() => {
            vec![response_text_item(role, text.clone())]
        }
        MessageContent::Text(_) => vec![],
        MessageContent::Parts(parts) => parts
            .iter()
            .filter_map(|part| match part {
                ContentPart::Text { text } if !text.is_empty() => {
                    Some(response_text_item(role, text.clone()))
                }
                ContentPart::Text { .. } => None,
                ContentPart::ImageUrl { image_url } => Some(ResponseContentItem::ImageUrl {
                    image_url: ResponseImageUrl {
                        url: image_url.url.clone(),
                    },
                }),
            })
            .collect(),
    }
}

pub(crate) fn split_instructions_and_input(messages: &[ModelMessage]) -> (String, Vec<Value>) {
    let mut instructions = Vec::new();
    let mut input = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        match message.role.as_str() {
            "system" => {
                if let Some(text) = message.content.as_ref().and_then(|c| c.text())
                    && !text.trim().is_empty()
                {
                    instructions.push(text);
                }
            }
            "tool" => {
                let output = message
                    .content
                    .as_ref()
                    .and_then(|c| c.text())
                    .unwrap_or_default();
                input.push(
                    serde_json::to_value(ResponseInputItem::FunctionCallOutput {
                        call_id: normalized_tool_call_id(message.tool_call_id.as_deref(), index),
                        output,
                    })
                    .expect("function_call_output should serialize"),
                );
            }
            role => {
                if let Some(content) = &message.content {
                    let items = content_to_response_items(role, content);
                    if !items.is_empty() {
                        input.push(
                            serde_json::to_value(ResponseInputItem::Message {
                                role: role.to_string(),
                                content: items,
                            })
                            .expect("message item should serialize"),
                        );
                    }
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

pub(crate) fn convert_tool_definition(tool: &Value) -> Value {
    if tool.get("type").and_then(Value::as_str) == Some("function")
        && let Some(function) = tool.get("function")
    {
        return json!({
            "type": "function",
            "name": function.get("name").cloned().unwrap_or(Value::String(String::new())),
            "description": function.get("description").cloned().unwrap_or(Value::Null),
            "parameters": function.get("parameters").cloned().unwrap_or_else(|| json!({ "type": "object", "properties": {}, "required": [], "additionalProperties": false })),
            "strict": function.get("strict").cloned().unwrap_or(Value::Bool(true)),
        });
    }
    tool.clone()
}
