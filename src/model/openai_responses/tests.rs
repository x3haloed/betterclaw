use super::decode::*;
use super::payload::*;

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::{
        convert_tool_definition, decode_responses_json, decode_responses_stream_frame,
        split_instructions_and_input,
    };
    use crate::model::{
        ModelEvent, ModelMessage, ModelToolCallMessage, ModelToolFunctionMessage, ReasoningMode,
    };

    #[test]
    fn translates_messages_to_instructions_and_input() {
        let (instructions, input) = split_instructions_and_input(&[
            ModelMessage {
                role: "system".to_string(),
                content: Some("be careful".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
            ModelMessage {
                role: "user".to_string(),
                content: Some("hello".to_string()),
                tool_calls: None,
                tool_call_id: None,
            },
        ]);
        assert_eq!(instructions, "be careful");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
    }

    #[test]
    fn translates_tool_calls_and_tool_outputs() {
        let (_instructions, input) = split_instructions_and_input(&[
            ModelMessage {
                role: "assistant".to_string(),
                content: None,
                tool_calls: Some(vec![ModelToolCallMessage {
                    id: "call-1".to_string(),
                    kind: "function".to_string(),
                    function: ModelToolFunctionMessage {
                        name: "echo".to_string(),
                        arguments: "{\"message\":\"hi\"}".to_string(),
                    },
                }]),
                tool_call_id: None,
            },
            ModelMessage {
                role: "tool".to_string(),
                content: Some("{\"message\":\"hi\"}".to_string()),
                tool_calls: None,
                tool_call_id: Some("call-1".to_string()),
            },
        ]);
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[1]["type"], "function_call_output");
        assert_eq!(input[1]["call_id"], "call-1");
    }

    #[test]
    fn converts_chat_style_tools_to_responses_tools() {
        let converted = convert_tool_definition(&json!({
            "type": "function",
            "function": {
                "name": "echo",
                "description": "Prints a message",
                "parameters": { "type": "object", "properties": { "message": { "type": "string" } } }
            }
        }));
        assert_eq!(converted["type"], "function");
        assert_eq!(converted["name"], "echo");
        assert_eq!(converted["strict"], true);
    }

    #[test]
    fn decodes_responses_json_tool_calls() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_responses_json(
            &json!({
                "output": [{
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call-1",
                    "name": "echo",
                    "arguments": "{\"message\":\"hi\"}",
                    "status": "completed"
                }],
                "usage": { "input_tokens": 5, "output_tokens": 2 }
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallArgumentsDelta { key, text } if key == "fc_1" && text == "{\"message\":\"hi\"}"
        )));
    }

    #[test]
    fn decodes_responses_sse_frames() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_responses_stream_frame(
            &json!({
                "type": "response.function_call_arguments.delta",
                "item_id": "fc_1",
                "delta": "{\"path\":\"README.md\"}"
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallArgumentsDelta { key, text } if key == "fc_1" && text == "{\"path\":\"README.md\"}"
        )));
    }

    #[test]
    fn decodes_responses_completion_frame() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_responses_stream_frame(
            &json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [{
                        "type": "message",
                        "id": "msg_1",
                        "content": [{ "type": "output_text", "text": "done" }]
                    }],
                    "usage": { "input_tokens": 7, "output_tokens": 3 }
                }
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::TextSnapshot { text } if text == "done"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Completed { finish_reason } if finish_reason.is_none()
        )));
    }
}
