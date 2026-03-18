use super::*;

#[cfg(test)]
mod tests {
    use std::fs;
    use std::sync::OnceLock;
    use std::time::{Duration, Instant};
    use serde_json::json;
    use tokio::sync::Mutex;
    use crate::model::MessageContent;

    use tempfile::tempdir;

    use super::{ProviderPreset, Runtime};
    use crate::channel::InboundEvent;
    use crate::db::Db;
    use crate::event::EventKind;
    use crate::model::{
        ModelEngine, ModelMessage, StubModelEngine, strip_reasoning_tags,
        validate_strict_schema,
    };
    use crate::turn::TurnStatus;
    use crate::workspace::Workspace;

    fn env_mutex() -> &'static Mutex<()> {
        static ENV_MUTEX: OnceLock<Mutex<()>> = OnceLock::new();
        ENV_MUTEX.get_or_init(|| Mutex::new(()))
    }

    #[tokio::test]
    async fn thread_resolution_is_stable() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let first = runtime
            .handle_inbound(InboundEvent::web("default", "thread-1", "hello"))
            .await
            .unwrap();
        let second = runtime
            .handle_inbound(InboundEvent::web("default", "thread-1", "again"))
            .await
            .unwrap();

        assert_eq!(first.thread.id, second.thread.id);
    }

    #[tokio::test]
    async fn tool_calls_continue_to_a_followup_model_response() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let outcome = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-tools",
                "/tool echo {\"message\":\"hi\"}",
            ))
            .await
            .unwrap();

        assert!(outcome.response.contains("\"message\":\"hi\""));
        let traces = runtime.list_turn_traces(&outcome.turn_id).await.unwrap();
        assert_eq!(traces.len(), 2);
    }

    #[tokio::test]
    async fn invalid_tool_parameters_are_fed_back_to_model() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("invalid-tool-feedback.db"))
            .await
            .unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let outcome = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-invalid-tool-feedback",
                "/tool tidepool_my_dashboard {\"domain_id\":\"abc\"}",
            ))
            .await
            .unwrap();

        assert!(outcome.response.contains("invalid_tool_parameters"));
        let timeline = runtime
            .list_thread_timeline(&outcome.thread.id)
            .await
            .unwrap();
        let tool_result_payload = timeline
            .iter()
            .find(|event| event.kind == EventKind::ToolResult)
            .map(|event| event.payload.clone())
            .expect("tool result should be recorded");
        assert_eq!(
            tool_result_payload["output"]["error"],
            json!("invalid_tool_parameters")
        );
        assert_eq!(
            tool_result_payload["output"]["tool"],
            json!("tidepool_my_dashboard")
        );
    }

    #[tokio::test]
    async fn parallel_tool_calls_continue_in_one_batch() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("test.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let outcome = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-parallel-tools",
                "/tool-batch [{\"name\":\"echo\",\"arguments\":{\"message\":\"one\"}},{\"name\":\"echo\",\"arguments\":{\"message\":\"two\"}}]",
            ))
            .await
            .unwrap();

        assert!(outcome.response.contains("\"message\":\"one\""));
        assert!(outcome.response.contains("\"message\":\"two\""));
        let timeline = runtime
            .list_thread_timeline(&outcome.thread.id)
            .await
            .unwrap();
        let tool_call_events = timeline
            .iter()
            .filter(|event| event.kind == EventKind::ToolCall)
            .count();
        let tool_result_events = timeline
            .iter()
            .filter(|event| event.kind == EventKind::ToolResult)
            .count();
        assert_eq!(tool_call_events, 2);
        assert_eq!(tool_result_events, 2);
    }

    #[tokio::test]
    async fn ask_user_tool_marks_turn_as_awaiting_user() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("ask-user.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let outcome = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-ask-user",
                "/tool ask_user {\"question\":\"Which branch should I use?\"}",
            ))
            .await
            .unwrap();

        assert_eq!(outcome.status, TurnStatus::AwaitingUser);
        assert_eq!(outcome.response, "Which branch should I use?");
        assert_eq!(
            outcome.outbound_messages,
            vec!["Which branch should I use?".to_string()]
        );
        let timeline = runtime
            .list_thread_timeline(&outcome.thread.id)
            .await
            .unwrap();
        assert!(
            timeline
                .iter()
                .any(|event| event.kind == EventKind::AwaitingUser)
        );
    }

    #[tokio::test]
    async fn tool_enabled_requests_require_a_tool_choice_and_include_final_message() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("tool-choice.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();
        let settings = runtime.get_runtime_settings("default").await.unwrap();

        let request = runtime.build_model_request(
            vec![ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text("hello".to_string())),
                tool_calls: None,
                tool_call_id: None,
            }],
            true,
            &settings,
        );

        assert_eq!(request.extra["tool_choice"], json!("required"));
        assert!(request.tools.iter().any(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
                == Some("final_message")
        }));
        assert!(request.tools.iter().any(|tool| {
            tool.get("function")
                .and_then(|function| function.get("name"))
                .and_then(serde_json::Value::as_str)
                == Some("no_op")
        }));
    }

    #[tokio::test]
    async fn final_message_tool_ends_turn_without_followup_model_call() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("final-message.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let outcome = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-final-message",
                "/final-message Done reading the files.",
            ))
            .await
            .unwrap();

        assert_eq!(outcome.status, TurnStatus::Succeeded);
        assert_eq!(outcome.response, "Done reading the files.");
        let traces = runtime.list_turn_traces(&outcome.turn_id).await.unwrap();
        assert_eq!(traces.len(), 1);
        let timeline = runtime
            .list_thread_timeline(&outcome.thread.id)
            .await
            .unwrap();
        assert!(timeline.iter().any(|event| event.kind == EventKind::ToolCall));
        assert!(timeline.iter().any(|event| event.kind == EventKind::ToolResult));
    }

    #[tokio::test]
    async fn final_message_is_replayed_as_normal_assistant_history() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("final-message-history.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let first = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-final-history",
                "/final-message I checked the file and found the orientation map.",
            ))
            .await
            .unwrap();

        let turns = runtime.list_thread_turns(&first.thread.id).await.unwrap();
        assert_eq!(turns.len(), 1);
        assert_eq!(
            turns[0].assistant_message.as_deref(),
            Some("I checked the file and found the orientation map.")
        );

        let second = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-final-history",
                "What did you find?",
            ))
            .await
            .unwrap();

        let traces = runtime.list_turn_traces(&second.turn_id).await.unwrap();
        assert_eq!(traces.len(), 1);
        let detail = runtime
            .get_trace_detail(&traces[0].id)
            .await
            .unwrap()
            .expect("trace detail");
        let messages = detail.request_body["messages"]
            .as_array()
            .expect("request messages array");

        assert!(messages.iter().any(|message| {
            message.get("role").and_then(serde_json::Value::as_str) == Some("assistant")
                && message.get("content").and_then(serde_json::Value::as_str)
                    == Some("I checked the file and found the orientation map.")
        }));
        assert!(!messages.iter().any(|message| {
            message.get("role").and_then(serde_json::Value::as_str) == Some("tool")
        }));
    }

    #[tokio::test]
    async fn nonterminal_tool_chain_gets_synthetic_summary_followup() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("tool-summary-repair.db"))
            .await
            .unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let outcome = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-tool-summary-repair",
                "/tool-summary-repair",
            ))
            .await
            .unwrap();

        assert_eq!(outcome.status, TurnStatus::Succeeded);
        assert!(outcome.response.contains("Summary:"));
        assert!(outcome.response.contains("\"message\":\"hi\""));

        let traces = runtime.list_turn_traces(&outcome.turn_id).await.unwrap();
        assert_eq!(traces.len(), 3);

        let final_trace = runtime
            .get_trace_detail(&traces[2].id)
            .await
            .unwrap()
            .expect("final trace detail");
        let messages = final_trace.request_body["messages"]
            .as_array()
            .expect("request messages array");
        assert!(messages.iter().any(|message| {
            message.get("role").and_then(serde_json::Value::as_str) == Some("user")
                && message.get("content").and_then(serde_json::Value::as_str)
                    == Some("Summarize what you just did for the user in one concise response. If you are done, call final_message with the user-facing summary.")
        }));
    }

    #[tokio::test]
    async fn assistant_messages_saved_without_reasoning_tags() {
        let _guard = env_mutex().lock().await;
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("history.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let outcome = runtime
            .handle_inbound(InboundEvent::web("default", "thread-1", "Hello"))
            .await
            .unwrap();
        assert!(!outcome.response.contains("<think>"));

        let turns = runtime.list_thread_turns("thread-1").await.unwrap();
        let assistant = turns[0].assistant_message.clone().unwrap();
        assert_eq!(assistant, strip_reasoning_tags(&assistant));
    }

    #[tokio::test]
    async fn auto_distill_uses_model_driven_compressor_output() {
        // Serialize to avoid SQLite contention from parallel Runtime::new calls.
        let _guard = env_mutex().lock().await;
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("compressor.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-compressor",
                "Please remember this behavior.",
            ))
            .await
            .unwrap();

        let wake_pack = runtime
            .db()
            .latest_memory_artifact("default", crate::memory::MemoryArtifactKind::WakePackV0)
            .await
            .unwrap()
            .unwrap();
        assert!(wake_pack.content.contains("Stub compressor wake pack"));

        let self_invariants = runtime
            .db()
            .list_memory_artifacts(
                "default",
                Some(crate::memory::MemoryArtifactKind::InvariantSelfV0),
                10,
            )
            .await
            .unwrap();
        assert!(!self_invariants.is_empty());
    }

    #[tokio::test]
    async fn replay_turn_creates_a_fresh_turn_with_replay_event() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("replay.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let original = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-replay",
                "hello replay",
            ))
            .await
            .unwrap();
        let replayed = runtime.replay_turn(&original.turn_id).await.unwrap();

        assert_eq!(replayed.thread.id, original.thread.id);
        assert_ne!(replayed.turn_id, original.turn_id);

        let timeline = runtime
            .list_thread_timeline(&original.thread.id)
            .await
            .unwrap();
        assert!(timeline.iter().any(|event| {
            event.turn_id == replayed.turn_id && event.kind == EventKind::ReplayRequested
        }));
    }

    #[tokio::test]
    async fn startup_recovery_marks_running_turns_failed() {
        let dir = tempdir().unwrap();
        let db_path = dir.path().join("recovery.db");
        let db = Db::open(&db_path).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();
        let thread = runtime
            .create_web_thread(Some("Recovery".to_string()))
            .await
            .unwrap();
        let turn = runtime
            .db()
            .create_turn(&thread.id, "stuck message", None)
            .await
            .unwrap();
        drop(runtime);

        let db = Db::open(&db_path).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();
        let recovered = runtime.get_turn(&turn.id).await.unwrap().unwrap();
        assert_eq!(recovered.status, TurnStatus::Failed);
        assert!(
            recovered
                .error
                .unwrap()
                .contains("Recovered abandoned running turn")
        );
        let timeline = runtime.list_thread_timeline(&thread.id).await.unwrap();
        assert!(
            timeline.iter().any(|event| {
                event.turn_id == turn.id && event.kind == EventKind::TurnRecovered
            })
        );
    }

    #[tokio::test]
    async fn rate_limited_turn_retries_after_retry_after_window() {
        // Serialize to avoid timing interference with parallel rate-limit tests.
        let _guard = env_mutex().lock().await;
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("rate-limit.db")).await.unwrap();
        let runtime = Runtime::with_model_engine_and_backoff(
            db,
            ModelEngine::stub(StubModelEngine::default()),
            "stub-model",
            "stub",
            Duration::from_millis(10),
        )
        .await
        .unwrap();

        let started = Instant::now();
        let outcome = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-rate-limit",
                "/rate-limit-once 25",
            ))
            .await
            .unwrap();

        assert!(started.elapsed() >= Duration::from_millis(20));
        assert!(outcome.response.contains("/rate-limit-once 25"));
        let traces = runtime.list_turn_traces(&outcome.turn_id).await.unwrap();
        assert_eq!(traces.len(), 2);
        let timeline = runtime
            .list_thread_timeline(&outcome.thread.id)
            .await
            .unwrap();
        assert!(
            timeline
                .iter()
                .any(|event| event.kind == EventKind::RateLimited)
        );
    }

    #[tokio::test]
    async fn rate_limit_gate_blocks_other_requests_until_retry_window() {
        // Serialize with other env-dependent tests to avoid parallel interference
        // with shared stub state and timing-sensitive assertions.
        let _guard = env_mutex().lock().await;

        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("rate-limit-gate.db"))
            .await
            .unwrap();
        let runtime = Runtime::with_model_engine_and_backoff(
            db,
            ModelEngine::stub(StubModelEngine::default()),
            "stub-model",
            "stub",
            Duration::from_millis(10),
        )
        .await
        .unwrap();

        let first_runtime = runtime.clone();
        let first = tokio::spawn(async move {
            first_runtime
                .handle_inbound(InboundEvent::web(
                    "default",
                    "thread-rate-limit-gate-a",
                    "/rate-limit-once 400",
                ))
                .await
                .unwrap()
        });

        // Give the first request time to acquire the gate and hit the rate limit
        // before the second request arrives.
        tokio::time::sleep(Duration::from_millis(200)).await;

        let second_started = Instant::now();
        let second = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-rate-limit-gate-b",
                "hello while blocked",
            ))
            .await
            .unwrap();
        let first = first.await.unwrap();

        // The second request should have been blocked for at least the remaining
        // throttle window. With a 400ms retry_after and 200ms initial sleep,
        // ~200ms should remain.
        assert!(
            second_started.elapsed() >= Duration::from_millis(100),
            "second request should have been blocked by shared throttle, waited {:?}",
            second_started.elapsed()
        );
        assert!(first.response.contains("/rate-limit-once 400"));
        assert!(second.response.contains("hello while blocked"));

        let second_timeline = runtime
            .list_thread_timeline(&second.thread.id)
            .await
            .unwrap();
        assert!(
            second_timeline.iter().any(|event| {
                event.kind == EventKind::RateLimited
                    && event
                        .payload
                        .get("shared_gate")
                        .and_then(serde_json::Value::as_bool)
                        == Some(true)
            }),
            "second thread timeline should contain a shared_gate RateLimited event; got events: {:?}",
            second_timeline.iter().map(|e| (&e.kind, &e.payload)).collect::<Vec<_>>()
        );
    }

    #[tokio::test]
    async fn missing_retry_after_uses_exponential_backoff_base() {
        // Serialize to avoid timing interference with parallel rate-limit tests.
        let _guard = env_mutex().lock().await;
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("rate-limit-backoff.db"))
            .await
            .unwrap();
        let runtime = Runtime::with_model_engine_and_backoff(
            db,
            ModelEngine::stub(StubModelEngine::default()),
            "stub-model",
            "stub",
            Duration::from_millis(15),
        )
        .await
        .unwrap();

        let started = Instant::now();
        let outcome = runtime
            .handle_inbound(InboundEvent::web(
                "default",
                "thread-rate-limit-backoff",
                "/rate-limit-backoff-once",
            ))
            .await
            .unwrap();

        assert!(started.elapsed() >= Duration::from_millis(12));
        assert!(outcome.response.contains("/rate-limit-backoff-once"));
    }

    #[test]
    fn provider_selection_defaults_to_local_chat_completions() {
        let _guard = env_mutex().blocking_lock();
        unsafe {
            std::env::remove_var("BETTERCLAW_PROVIDER");
            std::env::remove_var("BETTERCLAW_PROVIDER_MODE");
            std::env::remove_var("BETTERCLAW_MODEL");
            std::env::remove_var("BETTERCLAW_MODEL_BASE_URL");
        }
        let resolved = ProviderPreset::from_env().unwrap();
        assert_eq!(resolved.engine.kind_name(), "openai_chat_completions");
        assert_eq!(resolved.model_name, "qwen/qwen3.5-9b");
    }

    #[test]
    fn provider_selection_supports_openrouter_responses() {
        let _guard = env_mutex().blocking_lock();
        unsafe {
            std::env::set_var("BETTERCLAW_PROVIDER", "openrouter");
            std::env::set_var("BETTERCLAW_PROVIDER_MODE", "responses");
            std::env::set_var("OPENROUTER_MODEL", "anthropic/claude-sonnet-4");
            std::env::remove_var("OPENROUTER_API_KEY");
        }
        let resolved = ProviderPreset::from_env().unwrap();
        assert_eq!(resolved.engine.kind_name(), "openai_responses");
        assert_eq!(resolved.model_name, "anthropic/claude-sonnet-4");
        unsafe {
            std::env::remove_var("BETTERCLAW_PROVIDER");
            std::env::remove_var("BETTERCLAW_PROVIDER_MODE");
            std::env::remove_var("OPENROUTER_MODEL");
        }
    }

    #[test]
    fn provider_selection_supports_codex() {
        let _guard = env_mutex().blocking_lock();
        let dir = tempdir().unwrap();
        let auth_path = dir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            r#"{"tokens":{"access_token":"test-access-token","account_id":"acct_123"}}"#,
        )
        .unwrap();
        unsafe {
            std::env::set_var("BETTERCLAW_PROVIDER", "codex");
            std::env::set_var("OPENAI_CODEX_AUTH_PATH", &auth_path);
            std::env::set_var("OPENAI_CODEX_MODEL", "gpt-5-codex");
        }
        let resolved = ProviderPreset::from_env().unwrap();
        assert_eq!(resolved.engine.kind_name(), "openai_responses");
        assert_eq!(resolved.model_name, "gpt-5-codex");
        unsafe {
            std::env::remove_var("BETTERCLAW_PROVIDER");
            std::env::remove_var("OPENAI_CODEX_AUTH_PATH");
            std::env::remove_var("OPENAI_CODEX_MODEL");
        }
    }

    #[tokio::test]
    async fn tool_definitions_emit_strict_nullable_schemas() {
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("tool-schema.db")).await.unwrap();
        let runtime = Runtime::new(db).await.unwrap();

        let ask_user = runtime
            .tool_definitions()
            .into_iter()
            .find(|tool| {
                tool.get("function")
                    .and_then(|function| function.get("name"))
                    .and_then(serde_json::Value::as_str)
                    == Some("ask_user")
            })
            .expect("ask_user tool definition should exist");

        let function = ask_user.get("function").unwrap();
        assert_eq!(function.get("strict"), Some(&serde_json::Value::Bool(true)));

        let required = function
            .get("parameters")
            .and_then(|parameters| parameters.get("required"))
            .and_then(serde_json::Value::as_array)
            .unwrap()
            .iter()
            .filter_map(serde_json::Value::as_str)
            .collect::<Vec<_>>();
        assert!(required.contains(&"question"));
        assert!(required.contains(&"context"));

        assert_eq!(
            function
                .get("parameters")
                .and_then(|parameters| parameters.get("properties"))
                .and_then(|properties| properties.get("context"))
                .and_then(|context| context.get("type")),
            Some(&serde_json::json!(["string", "null"]))
        );
        validate_strict_schema(function.get("parameters").unwrap(), "ask_user")
            .expect("emitted tool parameters should validate in strict mode");
    }

    #[tokio::test]
    async fn startup_system_prompt_override_updates_runtime_settings() {
        let _guard = env_mutex().lock().await;
        let dir = tempdir().unwrap();
        let db = Db::open(&dir.path().join("prompt-override.db"))
            .await
            .unwrap();
        unsafe {
            std::env::set_var(
                "BETTERCLAW_SYSTEM_PROMPT",
                "You are QwenScout, the repo-mapper.",
            );
        }
        let runtime = Runtime::new(db).await.unwrap();
        let settings = runtime.get_runtime_settings("default").await.unwrap();
        assert_eq!(
            settings.system_prompt,
            "You are QwenScout, the repo-mapper."
        );
        unsafe {
            std::env::remove_var("BETTERCLAW_SYSTEM_PROMPT");
        }
    }

    #[tokio::test]
    async fn system_prompt_includes_workspace_agents_and_soul_files() {
        // Serialize to avoid races with env var mutations and Runtime init from parallel tests.
        let _guard = env_mutex().lock().await;
        let dir = tempdir().unwrap();
        fs::write(
            dir.path().join("AGENTS.md"),
            "## CONTEXT\n- User: Champ\n- Constraint: keep coordination cost low.",
        )
        .unwrap();
        fs::write(
            dir.path().join("SOUL.md"),
            "## VALUES\n- Prefer reversible probes.\n- Capture clarity.",
        )
        .unwrap();

        let db = Db::open(&dir.path().join("workspace-prompt.db"))
            .await
            .unwrap();
        let runtime = Runtime::new(db).await.unwrap();
        let settings = runtime.get_runtime_settings("default").await.unwrap();
        let workspace = Workspace::new("default", dir.path());

        let messages = runtime
            .build_system_messages(&settings, &workspace, Some("hello"))
            .await
            .unwrap();
        let system_prompt: String = messages
            .first()
            .and_then(|message| message.content.as_ref().and_then(|c| c.text()))
            .expect("system prompt should be present");

        assert!(system_prompt.contains("## Agent Instructions"));
        assert!(system_prompt.contains("Constraint: keep coordination cost low."));
        assert!(system_prompt.contains("## Core Values"));
        assert!(system_prompt.contains("Prefer reversible probes."));
        assert!(system_prompt.contains("You are BetterClaw Agent, a secure autonomous assistant."));
    }
}

#[cfg(test)]
mod content_tests {
    use chrono::Utc;
    use super::internal::build_user_message_content;
    use crate::channel::InboundAttachment;
    use crate::model::{ContentPart, MessageContent};
    use crate::turn::{Turn, TurnStatus};

    fn make_turn(user_message: &str, attachments: Option<&str>) -> Turn {
        Turn {
            id: "test-turn".to_string(),
            thread_id: "test-thread".to_string(),
            status: TurnStatus::Running,
            user_message: user_message.to_string(),
            attachments_json: attachments.map(|s| s.to_string()),
            assistant_message: None,
            error: None,
            created_at: Utc::now(),
            updated_at: Utc::now(),
        }
    }

    #[test]
    fn text_only_turn_produces_text_content() {
        let turn = make_turn("hello world", None);
        match build_user_message_content(&turn) {
            MessageContent::Text(text) => assert_eq!(text, "hello world"),
            _ => panic!("expected Text variant"),
        }
    }

    #[test]
    fn turn_with_image_attachments_produces_parts() {
        let attachments = serde_json::to_string(&vec![InboundAttachment {
            url: "https://example.com/photo.png".to_string(),
            filename: "photo.png".to_string(),
            content_type: Some("image/png".to_string()),
        }])
        .unwrap();
        let turn = make_turn("what is this?", Some(&attachments));
        match build_user_message_content(&turn) {
            MessageContent::Parts(parts) => {
                assert_eq!(parts.len(), 2);
                assert!(matches!(&parts[0], ContentPart::Text { text } if text == "what is this?"));
                match &parts[1] {
                    ContentPart::ImageUrl { image_url } => {
                        assert_eq!(image_url.url, "https://example.com/photo.png");
                    }
                    _ => panic!("expected ImageUrl part"),
                }
            }
            _ => panic!("expected Parts variant"),
        }
    }

    #[test]
    fn turn_with_non_image_attachments_stays_text() {
        let attachments = serde_json::to_string(&vec![InboundAttachment {
            url: "https://example.com/doc.pdf".to_string(),
            filename: "doc.pdf".to_string(),
            content_type: Some("application/pdf".to_string()),
        }])
        .unwrap();
        let turn = make_turn("read this", Some(&attachments));
        match build_user_message_content(&turn) {
            MessageContent::Text(text) => assert_eq!(text, "read this"),
            _ => panic!("expected Text variant for non-image attachments"),
        }
    }
}
