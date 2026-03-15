use crate::model::ModelMessage;
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
struct ResponseContentItem {
    #[serde(rename = "type")]
    kind: &'static str,
    text: String,
}

pub(crate) fn split_instructions_and_input(messages: &[ModelMessage]) -> (String, Vec<Value>) {
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

pub(crate) fn convert_tool_definition(tool: &Value) -> Value {
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
