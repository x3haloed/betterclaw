use super::decode::*;
use super::*;

#[cfg(test)]
mod tests {
    use std::time::Instant;

    use crate::model::{ContentPart, MessageContent};
    use serde_json::{Value, json};
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::net::TcpListener;

    use super::{
        OpenAiChatCompletionsEngine, decode_openai_response_json, decode_openai_stream_frame,
        parse_sse_data, serialize_chat_message,
    };
    use crate::model::{
        ModelEvent, ModelExchangeRequest, ModelMessage, ModelRunner, ModelToolCallMessage,
        ModelToolFunctionMessage, OpenAiCompatibleConfig, ReasoningMode,
    };

    #[test]
    fn decodes_non_streaming_tool_calls() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_openai_response_json(
            &json!({
                "choices": [{
                    "message": {
                        "tool_calls": [{
                            "id": "call-1",
                            "type": "function",
                            "function": {
                                "name": "echo",
                                "arguments": "{\"message\":\"hi\"}"
                            }
                        }]
                    },
                    "finish_reason": "tool_calls"
                }],
                "usage": {
                    "prompt_tokens": 12,
                    "completion_tokens": 3
                }
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallArgumentsDelta { text, .. } if text == "{\"message\":\"hi\"}"
        )));
    }

    #[test]
    fn decodes_streaming_tool_call_fragments() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_openai_stream_frame(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": {
                                "name": "read_file",
                                "arguments": "{\"path\":\"README.md\"}"
                            }
                        }]
                    }
                }]
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallNameDelta { text, .. } if text == "read_file"
        )));
    }

    #[test]
    fn streaming_tool_call_uses_index_for_followup_fragments() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let first = decode_openai_stream_frame(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "id": "call-1",
                            "function": { "name": "echo", "arguments": "" }
                        }]
                    }
                }]
            }),
            &mut reasoning_mode,
        );
        let second = decode_openai_stream_frame(
            &json!({
                "choices": [{
                    "delta": {
                        "tool_calls": [{
                            "index": 0,
                            "function": { "arguments": "{\"message\":\"hi\"}" }
                        }]
                    }
                }]
            }),
            &mut reasoning_mode,
        );
        assert!(first.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallStarted { key, id } if key == "0" && id.as_deref() == Some("call-1")
        )));
        assert!(second.iter().any(|event| matches!(
            event,
            ModelEvent::ToolCallArgumentsDelta { key, text } if key == "0" && text == "{\"message\":\"hi\"}"
        )));
    }

    #[test]
    fn detects_inline_reasoning_in_non_streaming_content() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_openai_response_json(
            &json!({
                "choices": [{
                    "message": {
                        "content": "<think>hidden</think>Visible"
                    },
                    "finish_reason": "stop"
                }]
            }),
            &mut reasoning_mode,
        );
        assert_eq!(reasoning_mode, ReasoningMode::InlineTagged);
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::TextSnapshot { text } if text == "<think>hidden</think>Visible"
        )));
    }

    #[test]
    fn assistant_tool_call_messages_include_null_content() {
        let value = serialize_chat_message(&ModelMessage {
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
        });
        assert_eq!(value.get("content"), Some(&Value::Null));
    }

    #[test]
    fn ordinary_messages_do_not_gain_null_content() {
        let value = serialize_chat_message(&ModelMessage {
            role: "assistant".to_string(),
            content: Some(MessageContent::Text("hello".to_string())),
            tool_calls: None,
            tool_call_id: None,
        });
        assert_eq!(value.get("content"), Some(&json!("hello")));
    }

    #[test]
    fn multipart_content_with_image_url_serializes_for_chat_completions() {
        let value = serialize_chat_message(&ModelMessage {
            role: "user".to_string(),
            content: Some(MessageContent::Parts(vec![
                ContentPart::text("what is this?"),
                ContentPart::image_url("https://example.com/photo.png"),
            ])),
            tool_calls: None,
            tool_call_id: None,
        });
        let content = value.get("content").expect("content present");
        let parts = content.as_array().expect("content is array");
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0].get("type").and_then(|v| v.as_str()), Some("text"));
        assert_eq!(
            parts[0].get("text").and_then(|v| v.as_str()),
            Some("what is this?")
        );
        assert_eq!(
            parts[1].get("type").and_then(|v| v.as_str()),
            Some("image_url")
        );
        let image_url = parts[1].get("image_url").expect("image_url field");
        assert_eq!(
            image_url.get("url").and_then(|v| v.as_str()),
            Some("https://example.com/photo.png")
        );
        assert_eq!(
            image_url.get("detail").and_then(|v| v.as_str()),
            Some("auto")
        );
    }

    #[test]
    fn detects_structured_reasoning_in_stream_frames() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_openai_stream_frame(
            &json!({
                "choices": [{
                    "delta": {
                        "reasoning_content": "hidden",
                        "content": "Visible"
                    }
                }]
            }),
            &mut reasoning_mode,
        );
        assert_eq!(reasoning_mode, ReasoningMode::Structured);
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::ReasoningDelta { text } if text == "hidden"
        )));
    }

    #[test]
    fn ignores_sse_comment_frames() {
        assert_eq!(parse_sse_data(": OPENROUTER PROCESSING"), None);
    }

    #[test]
    fn decodes_midstream_error_payloads() {
        let mut reasoning_mode = ReasoningMode::Unknown;
        let events = decode_openai_stream_frame(
            &json!({
                "error": { "message": "Provider disconnected unexpectedly" },
                "choices": [{
                    "delta": { "content": "" },
                    "finish_reason": "error"
                }]
            }),
            &mut reasoning_mode,
        );
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Failed { message } if message == "Provider disconnected unexpectedly"
        )));
        assert!(events.iter().any(|event| matches!(
            event,
            ModelEvent::Completed { finish_reason } if finish_reason.as_deref() == Some("error")
        )));
    }

    #[tokio::test]
    async fn streaming_returns_after_terminal_frame_without_done() {
        let listener = TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind listener");
        let addr = listener.local_addr().expect("listener addr");
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept");
            let mut request_buffer = [0_u8; 4096];
            let _ = socket
                .read(&mut request_buffer)
                .await
                .expect("read request");

            let body = concat!(
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"list_dir\",\"arguments\":\"{\\\"path\\\":\\\"src\\\"}\"}}]}}]}\n\n",
                "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            );
            let response = format!(
                "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\nconnection: keep-alive\r\ncontent-length: {}\r\n\r\n{}",
                body.len(),
                body,
            );
            socket
                .write_all(response.as_bytes())
                .await
                .expect("write response");
            tokio::time::sleep(std::time::Duration::from_secs(3)).await;
        });

        let engine = OpenAiChatCompletionsEngine::new(OpenAiCompatibleConfig {
            base_url: format!("http://{addr}/v1"),
            timeout: std::time::Duration::from_secs(5),
            provider_name: "test-provider".to_string(),
            bearer_token: None,
            extra_headers: Vec::new(),
        })
        .expect("engine");
        let request = ModelExchangeRequest {
            model: "test-model".to_string(),
            messages: vec![ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            tools: Vec::new(),
            max_tokens: None,
            stream: true,
            response_format: None,
            extra: json!({}),
        };

        let started = Instant::now();
        let result = engine.run(request).await.expect("streaming result");
        assert!(started.elapsed() < std::time::Duration::from_secs(3));
        assert_eq!(result.finish_reason.as_deref(), Some("tool_calls"));
        assert_eq!(result.tool_calls.len(), 1);

        server.abort();
        let _ = server.await;
    }
}
