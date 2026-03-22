use super::decode::*;
use super::payload::*;

#[cfg(test)]
mod tests {
    use crate::model::MessageContent;
    use serde_json::json;

    use super::{
        convert_tool_definition, decode_responses_json, decode_responses_stream_frame,
        parse_sse_data, split_instructions_and_input, take_sse_block,
    };
    use crate::model::normalize_schema_strict;
    use crate::model::openai_responses::responses_text_format;
    use crate::model::{
        ModelEvent, ModelExchangeRequest, ModelMessage, ModelToolCallMessage,
        ModelToolFunctionMessage, OpenAiCompatibleConfig, OpenAiResponsesEngine, ReasoningMode,
        validate_strict_schema,
    };

    fn required_names(value: &serde_json::Value) -> Vec<String> {
        let mut names = value
            .as_array()
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .map(ToString::to_string)
            .collect::<Vec<_>>();
        names.sort();
        names
    }

    #[test]
    fn translates_messages_to_instructions_and_input() {
        let (instructions, input) = split_instructions_and_input(&[
            ModelMessage {
                role: "system".to_string(),
                content: Some(MessageContent::Text("be careful".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
            ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
        ]);
        assert_eq!(instructions, "be careful");
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["type"], "input_text");
    }

    #[test]
    fn translates_assistant_history_as_output_text() {
        let (_instructions, input) = split_instructions_and_input(&[
            ModelMessage {
                role: "assistant".to_string(),
                content: Some(MessageContent::Text("already answered".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
            ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("follow up".to_string())),
                tool_calls: None,
                tool_call_id: None,
            },
        ]);

        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["content"][0]["type"], "output_text");
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["type"], "input_text");
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
                content: Some(MessageContent::Text("{\"message\":\"hi\"}".to_string())),
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
            ModelEvent::ToolCallArgumentsDelta { key, text } if key == "call-1" && text == "{\"message\":\"hi\"}"
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

    #[test]
    fn decodes_responses_sse_with_unstable_item_ids_using_output_index() {
        let payload = concat!(
            "event: response.output_item.added\n",
            "data: {\"type\":\"response.output_item.added\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_initial\",\"call_id\":\"call_123\",\"name\":\"\",\"arguments\":\"\"}}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"item_id\":\"fc_delta_a\",\"delta\":\"{\\\"path\\\":\"}\n\n",
            "event: response.function_call_arguments.delta\n",
            "data: {\"type\":\"response.function_call_arguments.delta\",\"output_index\":1,\"item_id\":\"fc_delta_b\",\"delta\":\"\\\"README.md\\\"}\"}\n\n",
            "event: response.output_item.done\n",
            "data: {\"type\":\"response.output_item.done\",\"output_index\":1,\"item\":{\"type\":\"function_call\",\"id\":\"fc_done\",\"call_id\":\"call_123\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}}\n\n",
            "event: response.completed\n",
            "data: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"id\":\"fc_completed\",\"call_id\":\"call_123\",\"name\":\"read_file\",\"arguments\":\"{\\\"path\\\":\\\"README.md\\\"}\"}]}}\n\n"
        );

        let mut buffer = payload.to_string();
        let mut reasoning_mode = ReasoningMode::Unknown;
        let mut events = Vec::new();
        while let Some((block, rest)) = take_sse_block(&buffer) {
            buffer = rest;
            let Some(data) = parse_sse_data(&block) else {
                continue;
            };
            let frame: serde_json::Value = serde_json::from_str(&data).expect("frame json");
            events.extend(decode_responses_stream_frame(&frame, &mut reasoning_mode));
        }

        let started_keys: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ToolCallStarted { key, .. } => Some(key.as_str()),
                _ => None,
            })
            .collect();
        assert!(started_keys.contains(&"output_index:1"));
        assert!(!started_keys.contains(&"fc_delta_a"));
        assert!(!started_keys.contains(&"fc_delta_b"));
        assert!(!started_keys.contains(&"fc_done"));

        let name_deltas: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ToolCallNameDelta { key, text } => Some((key.as_str(), text.as_str())),
                _ => None,
            })
            .collect();
        assert!(name_deltas.contains(&("output_index:1", "read_file")));

        let arg_deltas: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ToolCallArgumentsDelta { key, text } => {
                    Some((key.as_str(), text.as_str()))
                }
                _ => None,
            })
            .collect();
        assert!(arg_deltas.contains(&("output_index:1", "{\"path\":")));
        assert!(arg_deltas.contains(&("output_index:1", "\"README.md\"}")));

        let finished_keys: Vec<_> = events
            .iter()
            .filter_map(|event| match event {
                ModelEvent::ToolCallFinished { key } => Some(key.as_str()),
                _ => None,
            })
            .collect();
        assert!(finished_keys.contains(&"output_index:1"));
    }

    #[test]
    fn responses_text_format_flattens_chat_style_json_schema() {
        let format = responses_text_format(&json!({
            "type": "json_schema",
            "json_schema": {
                "name": "betterclaw_memory_distill",
                "schema": {
                    "type": "object",
                    "properties": {
                        "wake_pack": { "type": "string" }
                    },
                    "required": ["wake_pack"],
                    "additionalProperties": false
                },
                "strict": true
            }
        }));

        assert_eq!(format["type"], json!("json_schema"));
        assert_eq!(format["name"], json!("betterclaw_memory_distill"));
        assert_eq!(format["strict"], json!(true));
        assert_eq!(format["schema"]["type"], json!("object"));
    }

    #[test]
    fn normalize_response_schema_requires_nullable_optional_fields() {
        let schema = normalize_schema_strict(&json!({
            "type": "object",
            "properties": {
                "wake_pack": { "type": "string" },
                "summary": { "type": ["string", "null"] }
            },
            "required": ["wake_pack"]
        }));

        assert_eq!(
            required_names(&schema["required"]),
            vec!["summary".to_string(), "wake_pack".to_string()]
        );
        assert_eq!(
            schema["properties"]["summary"]["type"],
            json!(["string", "null"])
        );
        assert_eq!(schema["additionalProperties"], json!(false));
        validate_strict_schema(&schema, "response_schema").expect("schema should validate");
    }

    #[test]
    fn responses_payload_uses_flattened_text_format() {
        let engine = OpenAiResponsesEngine::new(OpenAiCompatibleConfig::default()).expect("engine");
        let payload = engine.build_payload(&ModelExchangeRequest { role: None,
            role: None,
            model: "gpt-5-mini".to_string(),
            messages: vec![ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Vec::new(),
            max_tokens: Some(128),
            stream: false,
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "betterclaw_memory_distill",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "wake_pack": { "type": "string" }
                        },
                        "required": ["wake_pack"],
                        "additionalProperties": false
                    }
                }
            })),
            extra: json!({}),
        });

        assert_eq!(payload["text"]["format"]["type"], json!("json_schema"));
        assert_eq!(
            payload["text"]["format"]["name"],
            json!("betterclaw_memory_distill")
        );
        assert!(payload["text"]["format"].get("json_schema").is_none());
        validate_strict_schema(
            &payload["text"]["format"]["schema"],
            "betterclaw_memory_distill",
        )
        .expect("payload schema should validate");
    }

    #[test]
    fn responses_payload_normalizes_schema_required_fields() {
        let engine = OpenAiResponsesEngine::new(OpenAiCompatibleConfig::default()).expect("engine");
        let payload = engine.build_payload(&ModelExchangeRequest { role: None,
            role: None,
            model: "gpt-5-mini".to_string(),
            messages: vec![ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Vec::new(),
            max_tokens: Some(128),
            stream: false,
            response_format: Some(json!({
                "type": "json_schema",
                "json_schema": {
                    "name": "betterclaw_memory_distill",
                    "schema": {
                        "type": "object",
                        "properties": {
                            "wake_pack": { "type": "string" },
                            "summary": { "type": ["string", "null"] }
                        },
                        "required": ["wake_pack"]
                    }
                }
            })),
            extra: json!({}),
        });

        assert_eq!(
            required_names(&payload["text"]["format"]["schema"]["required"]),
            vec!["summary".to_string(), "wake_pack".to_string()]
        );
        assert_eq!(
            payload["text"]["format"]["schema"]["properties"]["summary"]["type"],
            json!(["string", "null"])
        );
        validate_strict_schema(
            &payload["text"]["format"]["schema"],
            "betterclaw_memory_distill",
        )
        .expect("normalized payload schema should validate");
    }

    #[test]
    fn codex_payload_omits_max_output_tokens() {
        let engine = OpenAiResponsesEngine::new(OpenAiCompatibleConfig {
            provider_name: "codex".to_string(),
            ..OpenAiCompatibleConfig::default()
        })
        .expect("engine");
        let payload = engine.build_payload(&ModelExchangeRequest { role: None,
            role: None,
            model: "gpt-5.4-mini".to_string(),
            messages: vec![ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Vec::new(),
            max_tokens: Some(128),
            stream: false,
            response_format: None,
            extra: json!({}),
        });

        assert!(payload.get("max_output_tokens").is_none());
    }

    #[test]
    fn codex_payload_forces_streaming() {
        let engine = OpenAiResponsesEngine::new(OpenAiCompatibleConfig {
            provider_name: "codex".to_string(),
            ..OpenAiCompatibleConfig::default()
        })
        .expect("engine");
        let payload = engine.build_payload(&ModelExchangeRequest { role: None,
            role: None,
            model: "gpt-5.4-mini".to_string(),
            messages: vec![ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Vec::new(),
            max_tokens: None,
            stream: false,
            response_format: None,
            extra: json!({}),
        });

        assert_eq!(payload["stream"], json!(true));
    }

    #[test]
    fn effective_streaming_follows_payload_not_request_flag() {
        let engine = OpenAiResponsesEngine::new(OpenAiCompatibleConfig {
            provider_name: "codex".to_string(),
            ..OpenAiCompatibleConfig::default()
        })
        .expect("engine");
        let payload = engine.build_payload(&ModelExchangeRequest { role: None,
            role: None,
            model: "gpt-5.4-mini".to_string(),
            messages: vec![ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Vec::new(),
            max_tokens: None,
            stream: false,
            response_format: None,
            extra: json!({}),
        });

        assert!(OpenAiResponsesEngine::payload_requests_stream(&payload));
    }
}
