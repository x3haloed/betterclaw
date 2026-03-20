use super::memory::default_memory_namespace;
use super::*;
use crate::error::RuntimeError;
use crate::event::EventKind;
use crate::model::*;
use crate::routine::RoutineConfig;
use crate::skill::{build_skills_block, discover_skills};
use crate::thread::Thread;
use crate::tool::*;
use crate::turn::{Turn, TurnStatus};
use crate::workspace::Workspace;
use chrono::Utc;
use futures_util::future::join_all;
use serde_json::{Value, json};
use std::time::{Duration, Instant};
use tokio::fs;
use uuid::Uuid;

const SYNTHETIC_TOOL_SUMMARY_PROMPT: &str = "Summarize what you just did for the user in one concise response. If you are done, call final_message with the user-facing summary.";

impl Runtime {
    pub(crate) fn parse_tool_control(output: &Value) -> Option<ToolControl> {
        let control = output
            .get("control")
            .and_then(|value| value.get("__betterclaw_control"))
            .or_else(|| output.get("__betterclaw_control"))?;
        let kind = control.get("kind")?.as_str()?;
        let payload = control.get("payload")?;
        match kind {
            "message" => Some(ToolControl::Message {
                content: payload.get("content")?.as_str()?.to_string(),
            }),
            "ask_user" => Some(ToolControl::AskUser {
                question: payload.get("question")?.as_str()?.to_string(),
            }),
            "final_message" => Some(ToolControl::FinalMessage {
                content: payload.get("content")?.as_str()?.to_string(),
            }),
            _ => None,
        }
    }

    pub(crate) async fn apply_startup_setting_overrides(
        &self,
        agent_id: &str,
    ) -> Result<(), RuntimeError> {
        let Some(system_prompt) = system_prompt_override_from_env() else {
            return Ok(());
        };
        let Some(mut settings) = self
            .db
            .load_runtime_settings(agent_id)
            .await
            .map_err(RuntimeError::from)?
        else {
            return Ok(());
        };
        if settings.system_prompt == system_prompt {
            return Ok(());
        }
        settings.system_prompt = system_prompt;
        settings.updated_at = Utc::now();
        self.db
            .update_runtime_settings(&settings)
            .await
            .map_err(RuntimeError::from)?;
        Ok(())
    }

    pub(crate) async fn handle_inbound_internal(
        &self,
        event: InboundEvent,
        replay_source_turn_id: Option<String>,
    ) -> Result<TurnOutcome, RuntimeError> {
        let thread = self
            .resolve_thread(&event.agent_id, &event.channel, &event.external_thread_id)
            .await?;
        let workspace = self.workspace_for_agent(&event.agent_id).await?;
        let settings = self.get_runtime_settings(&event.agent_id).await?;
        let attachments_json = if event.attachments.is_empty() {
            None
        } else {
            Some(serde_json::to_string(&event.attachments).unwrap_or_else(|_| "[]".to_string()))
        };
        let turn = self
            .db
            .create_turn(&thread.id, &event.content, attachments_json.as_deref())
            .await
            .map_err(RuntimeError::from)?;

        self.append_event_and_publish(
            &turn.id,
            &thread.id,
            EventKind::InboundMessage,
            json!({
                "content": event.content,
                "received_at": event.received_at,
                "metadata": event.metadata,
            }),
        )
        .await?;
        self.append_event_and_publish(
            &turn.id,
            &thread.id,
            EventKind::ThreadResolved,
            json!({ "thread_id": thread.id, "external_thread_id": thread.external_thread_id }),
        )
        .await?;
        if let Some(source_turn_id) = replay_source_turn_id.clone() {
            self.append_event_and_publish(
                &turn.id,
                &thread.id,
                EventKind::ReplayRequested,
                json!({
                    "source_turn_id": source_turn_id,
                    "requested_at": Utc::now(),
                }),
            )
            .await?;
        }

        let thread_metadata = stable_channel_thread_metadata(&event);
        if thread.metadata != thread_metadata {
            self.db
                .update_thread_metadata(&thread.id, thread_metadata.as_ref())
                .await
                .map_err(RuntimeError::from)?;
        }
        let thread = self
            .db
            .get_thread(&thread.id)
            .await
            .map_err(RuntimeError::from)?
            .ok_or_else(|| RuntimeError::ThreadNotFound(thread.id.clone()))?;

        let mut conversation = self
            .build_conversation_history(&thread, &turn, &settings, &workspace)
            .await?;
        let initial_request = self.build_model_request(conversation.clone(), true, &settings);
        self.append_event_and_publish(
            &turn.id,
            &thread.id,
            EventKind::ContextAssembled,
            json!({
                "message_count": initial_request.messages.len(),
                "tool_count": initial_request.tools.len(),
                "model": initial_request.model,
                "stream": self.effective_stream_for_request(&initial_request),
            }),
        )
        .await?;

        let mut request = initial_request;
        let mut outbound_messages = Vec::new();
        let mut visible_reply_segments = Vec::new();
        let mut chain_ends_with_nonterminal_tool = false;
        let mut synthetic_summary_prompt_sent = false;
        let (final_response, last_trace_id, final_status) = loop {
            let exchange = self
                .run_and_record_exchange(&turn, &thread, &event.agent_id, &event.channel, request)
                .await?;
            let trace = self
                .record_trace(&turn, &thread, &event.agent_id, &event.channel, &exchange)
                .await?;
            let trace_id = trace.id;

            let visible_exchange_content = exchange
                .content
                .as_deref()
                .and_then(normalize_visible_reply_segment);
            if let Some(content) = visible_exchange_content.as_deref() {
                push_visible_reply_segment(&mut visible_reply_segments, content);
            }

            let recoverable_parse_error =
                exchange.outcome == TraceOutcome::ParseError && !exchange.tool_calls.is_empty();
            if exchange.outcome != TraceOutcome::Ok && !recoverable_parse_error {
                let error = exchange
                    .error_summary
                    .clone()
                    .unwrap_or_else(|| "model exchange failed".to_string());
                self.update_turn_and_publish(
                    &thread.id,
                    &turn.id,
                    TurnStatus::Failed,
                    None,
                    Some(error.clone()),
                )
                .await?;
                self.append_event_and_publish(
                    &turn.id,
                    &thread.id,
                    EventKind::Error,
                    json!({ "message": error }),
                )
                .await?;
                return Err(RuntimeError::ModelParse(
                    exchange
                        .error_summary
                        .unwrap_or_else(|| "model exchange failed".to_string()),
                ));
            }

            if exchange.tool_calls.is_empty() {
                if chain_ends_with_nonterminal_tool
                    && visible_exchange_content.is_none()
                    && !synthetic_summary_prompt_sent
                {
                    synthetic_summary_prompt_sent = true;
                    conversation.push(ModelMessage {
                        role: "user".to_string(),
                        content: Some(MessageContent::Text(
                            SYNTHETIC_TOOL_SUMMARY_PROMPT.to_string(),
                        )),
                        tool_calls: None,
                        tool_call_id: None,
                    });
                    request = self.build_model_request(conversation.clone(), true, &settings);
                    continue;
                }
                break (
                    compose_visible_reply(&visible_reply_segments, None),
                    trace_id,
                    TurnStatus::Succeeded,
                );
            }

            let continuation_messages = match self
                .execute_tool_calls(&turn, &thread, &workspace, exchange.tool_calls)
                .await
            {
                Ok(outcome) => outcome,
                Err(error) => {
                    self.update_turn_and_publish(
                        &thread.id,
                        &turn.id,
                        TurnStatus::Failed,
                        None,
                        Some(error.to_string()),
                    )
                    .await?;
                    self.append_event_and_publish(
                        &turn.id,
                        &thread.id,
                        EventKind::Error,
                        json!({ "message": error.to_string() }),
                    )
                    .await?;
                    return Err(error);
                }
            };
            if !continuation_messages.outbound_messages.is_empty() {
                for content in &continuation_messages.outbound_messages {
                    self.record_outbound_and_publish(
                        &turn,
                        &thread,
                        content,
                        event.metadata.clone(),
                    )
                    .await?;
                }
                outbound_messages.extend(continuation_messages.outbound_messages.clone());
            }
            if let Some(content) = continuation_messages.final_message {
                break (
                    compose_visible_reply(&visible_reply_segments, Some(content)),
                    trace_id,
                    TurnStatus::Succeeded,
                );
            }
            if let Some(question) = continuation_messages.ask_user_question {
                break (
                    compose_visible_reply(&visible_reply_segments, Some(question)),
                    trace_id,
                    TurnStatus::AwaitingUser,
                );
            }
            chain_ends_with_nonterminal_tool = true;
            synthetic_summary_prompt_sent = false;
            conversation.extend(continuation_messages.continuation_messages);
            request = self.build_model_request(conversation.clone(), true, &settings);
        };

        self.update_turn_and_publish(
            &thread.id,
            &turn.id,
            final_status.clone(),
            Some(final_response.clone()),
            None,
        )
        .await?;
        if final_status == TurnStatus::AwaitingUser {
            self.append_event_and_publish(
                &turn.id,
                &thread.id,
                EventKind::AwaitingUser,
                json!({ "question": final_response }),
            )
            .await?;
        }
        self.record_outbound_and_publish(&turn, &thread, &final_response, event.metadata.clone())
            .await?;
        if !final_response.trim().is_empty() {
            outbound_messages.push(final_response.clone());
        }
        let completed_turn = self
            .get_turn(&turn.id)
            .await?
            .ok_or_else(|| RuntimeError::TurnNotFound(turn.id.clone()))?;
        self.spawn_post_turn_maintenance(thread.clone(), completed_turn, settings.clone());

        Ok(TurnOutcome {
            thread,
            turn_id: turn.id,
            response: final_response,
            trace_id: last_trace_id,
            status: final_status,
            outbound_messages,
        })
    }

    fn spawn_post_turn_maintenance(
        &self,
        thread: Thread,
        completed_turn: Turn,
        settings: RuntimeSettings,
    ) {
        let runtime = self.clone();
        tokio::spawn(async move {
            if let Err(error) = runtime
                .sync_memory_for_turn(&thread, &completed_turn, &settings)
                .await
            {
                tracing::error!(
                    error = %error,
                    thread_id = %thread.id,
                    turn_id = %completed_turn.id,
                    "Post-turn memory sync failed"
                );
            }
            if settings.enable_observations {
                let _ = runtime
                    .run_observation_routines(
                        &default_memory_namespace(),
                        &RoutineConfig::default(),
                    )
                    .await;
            }
        });
    }

    pub(crate) async fn build_conversation_history(
        &self,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
        workspace: &Workspace,
    ) -> Result<Vec<ModelMessage>, RuntimeError> {
        let mut messages = self
            .build_system_messages(thread, settings, workspace, Some(&turn.user_message))
            .await?;
        let prior_turns = self
            .list_thread_turns(&thread.id)
            .await?
            .into_iter()
            .filter(|prior_turn| prior_turn.id != turn.id)
            .collect::<Vec<_>>();
        let history_limit = settings.max_history_turns as usize;
        let history_slice = if prior_turns.len() > history_limit {
            &prior_turns[prior_turns.len() - history_limit..]
        } else {
            &prior_turns[..]
        };
        for prior_turn in history_slice {
            messages.push(ModelMessage {
                role: "user".to_string(),
                content: Some(MessageContent::Text(prior_turn.user_message.clone())),
                tool_calls: None,
                tool_call_id: None,
            });
            if let Some(assistant_message) = prior_turn.assistant_message.clone() {
                messages.push(ModelMessage {
                    role: "assistant".to_string(),
                    content: Some(MessageContent::Text(strip_reasoning_tags(
                        &assistant_message,
                    ))),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        let user_content = build_user_message_content(turn);
        messages.push(ModelMessage {
            role: "user".to_string(),
            content: Some(user_content),
            tool_calls: None,
            tool_call_id: None,
        });
        Ok(messages)
    }

    pub(crate) async fn build_system_messages(
        &self,
        thread: &Thread,
        settings: &RuntimeSettings,
        workspace: &Workspace,
        query_hint: Option<&str>,
    ) -> Result<Vec<ModelMessage>, RuntimeError> {
        let mut messages = Vec::new();
        let combined_system_prompt = self.compose_system_prompt(settings, workspace).await?;
        if !combined_system_prompt.trim().is_empty() {
            messages.push(ModelMessage {
                role: "system".to_string(),
                content: Some(MessageContent::Text(combined_system_prompt)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        if let Some(channel_context) = build_channel_system_context_block(thread) {
            messages.push(ModelMessage {
                role: "system".to_string(),
                content: Some(MessageContent::Text(channel_context)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        let namespace = default_memory_namespace();
        if let Some(wake_pack_block) = self.current_wake_pack_block(settings).await? {
            messages.push(ModelMessage {
                role: "system".to_string(),
                content: Some(MessageContent::Text(wake_pack_block)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        if settings.inject_ledger_recall
            && let Some(recall_block) = self
                .build_ledger_recall_block(
                    &namespace,
                    query_hint.unwrap_or(&settings.system_prompt),
                )
                .await?
        {
            messages.push(ModelMessage {
                role: "system".to_string(),
                content: Some(MessageContent::Text(recall_block)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        if settings.inject_observations
            && let Some(obs_block) = self.build_observations_block(&namespace).await?
        {
            messages.push(ModelMessage {
                role: "system".to_string(),
                content: Some(MessageContent::Text(obs_block)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        if settings.inject_skills {
            let skills = discover_skills(&workspace.root).await;
            if let Some(skills_block) = build_skills_block(&skills) {
                messages.push(ModelMessage {
                    role: "system".to_string(),
                    content: Some(MessageContent::Text(skills_block)),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        Ok(messages)
    }

    pub(crate) async fn current_wake_pack_block(
        &self,
        settings: &RuntimeSettings,
    ) -> Result<Option<String>, RuntimeError> {
        if !settings.inject_wake_pack {
            return Ok(None);
        }
        let namespace = default_memory_namespace();
        let Some(wake_pack) = self
            .db
            .latest_memory_artifact(&namespace, MemoryArtifactKind::WakePackV0)
            .await
            .map_err(RuntimeError::from)?
        else {
            return Ok(None);
        };
        Ok(Some(format!(
            "<wake_pack>\n{}\n</wake_pack>",
            wake_pack.content
        )))
    }

    async fn compose_system_prompt(
        &self,
        settings: &RuntimeSettings,
        workspace: &Workspace,
    ) -> Result<String, RuntimeError> {
        let mut parts = Vec::new();
        if let Some(identity_prompt) = workspace_identity_prompt(workspace).await? {
            parts.push(identity_prompt);
        }
        if !settings.system_prompt.trim().is_empty() {
            parts.push(settings.system_prompt.clone());
        }
        Ok(parts.join("\n\n---\n\n"))
    }

    pub(crate) async fn build_ledger_recall_block(
        &self,
        namespace_id: &str,
        query: &str,
    ) -> Result<Option<String>, RuntimeError> {
        let hits = self.search_recall(namespace_id, query, 6).await?;
        if hits.is_empty() {
            return Ok(None);
        }
        let mut block =
            String::from("<ledger_recall>\nCandidate evidence from prior runtime history:\n");
        for hit in hits {
            block.push_str(&format!(
                "- [{}] {}\n",
                hit.citation.unwrap_or_else(|| hit.entry_id.clone()),
                hit.content.replace('\n', " ")
            ));
        }
        block.push_str("</ledger_recall>");
        Ok(Some(block))
    }

    pub(crate) fn build_model_request(
        &self,
        messages: Vec<ModelMessage>,
        allow_tools: bool,
        settings: &RuntimeSettings,
    ) -> ModelExchangeRequest {
        ModelExchangeRequest {
            model: self.model_name.clone(),
            messages,
            tools: if allow_tools && settings.allow_tools {
                self.tool_definitions()
            } else {
                Vec::new()
            },
            max_tokens: Some(settings.max_tokens),
            stream: settings.stream,
            response_format: None,
            extra: if allow_tools && settings.allow_tools {
                json!({ "tool_choice": "required" })
            } else {
                json!({})
            },
        }
    }

    pub(crate) fn effective_stream_for_request(&self, request: &ModelExchangeRequest) -> bool {
        request.stream || self.provider_name == "codex"
    }

    pub(crate) async fn run_and_record_exchange(
        &self,
        turn: &Turn,
        thread: &Thread,
        agent_id: &str,
        channel: &str,
        request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, RuntimeError> {
        let mut attempt = 0usize;
        loop {
            attempt += 1;
            if self.wait_for_provider_window(turn, thread, attempt).await? {
                continue;
            }
            let gate_wait_started = Instant::now();
            let _provider_gate = self.provider_request_gate.lock().await;
            let gate_wait = gate_wait_started.elapsed();
            if gate_wait >= Duration::from_millis(1) {
                self.append_event_and_publish(
                    &turn.id,
                    &thread.id,
                    EventKind::RateLimited,
                    json!({
                        "provider": self.provider_name,
                        "attempt": attempt,
                        "message": "waiting for shared provider gate",
                        "retry_after_ms": gate_wait.as_millis() as u64,
                        "resumes_at": Utc::now().to_rfc3339(),
                        "shared_gate": true,
                    }),
                )
                .await?;
            }
            if self.wait_for_provider_window(turn, thread, attempt).await? {
                continue;
            }
            match self.model_engine.run(request.clone()).await {
                Ok(exchange) => {
                    self.provider_throttle.note_success().await;
                    self.append_model_events(turn, thread, &exchange).await?;
                    return Ok(exchange);
                }
                Err(ModelEngineError::RateLimited {
                    message,
                    retry_after,
                    exchange,
                }) => {
                    let trace = self
                        .record_trace(turn, thread, agent_id, channel, exchange.as_ref())
                        .await?;
                    self.append_model_events(turn, thread, exchange.as_ref())
                        .await?;
                    let wait = self.provider_throttle.arm(retry_after).await;
                    self.append_event_and_publish(
                        &turn.id,
                        &thread.id,
                        EventKind::RateLimited,
                        json!({
                            "provider": self.provider_name,
                            "attempt": attempt,
                            "message": message,
                            "retry_after_ms": wait.as_millis() as u64,
                            "trace_id": trace.id,
                            "resumes_at": (Utc::now() + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::MAX)).to_rfc3339(),
                        }),
                    )
                    .await?;
                    let running_turns = self
                        .db
                        .list_running_turns()
                        .await
                        .map_err(RuntimeError::from)?;
                    for running_turn in running_turns
                        .into_iter()
                        .filter(|running_turn| running_turn.id != turn.id)
                    {
                        self.append_event_and_publish(
                            &running_turn.id,
                            &running_turn.thread_id,
                            EventKind::RateLimited,
                            json!({
                                "provider": self.provider_name,
                                "attempt": attempt,
                                "message": "waiting for shared provider backoff window",
                                "retry_after_ms": wait.as_millis() as u64,
                                "resumes_at": (Utc::now() + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::MAX)).to_rfc3339(),
                                "shared_gate": true,
                                "triggered_by_turn_id": turn.id,
                                "trace_id": trace.id,
                            }),
                        )
                        .await?;
                    }
                }
                Err(error) => {
                    self.record_trace(turn, thread, agent_id, channel, error.exchange())
                        .await?;
                    self.append_model_events(turn, thread, error.exchange())
                        .await?;
                    self.update_turn_and_publish(
                        &thread.id,
                        &turn.id,
                        TurnStatus::Failed,
                        None,
                        error.exchange().error_summary.clone(),
                    )
                    .await?;
                    self.append_event_and_publish(
                        &turn.id,
                        &thread.id,
                        EventKind::Error,
                        json!({ "message": error.to_string() }),
                    )
                    .await?;
                    return Err(RuntimeError::ModelEngine(error));
                }
            }
        }
    }

    pub(crate) async fn wait_for_provider_window(
        &self,
        turn: &Turn,
        thread: &Thread,
        attempt: usize,
    ) -> Result<bool, RuntimeError> {
        if let Some(wait) = self.provider_throttle.current_wait().await {
            self.append_event_and_publish(
                &turn.id,
                &thread.id,
                EventKind::RateLimited,
                json!({
                    "provider": self.provider_name,
                    "attempt": attempt,
                    "message": "waiting for shared provider backoff window",
                    "retry_after_ms": wait.as_millis() as u64,
                    "resumes_at": (Utc::now() + chrono::Duration::from_std(wait).unwrap_or(chrono::Duration::MAX)).to_rfc3339(),
                    "shared_gate": true,
                }),
            )
            .await?;
            tokio::time::sleep(wait).await;
            return Ok(true);
        }
        Ok(false)
    }

    pub(crate) async fn append_model_events(
        &self,
        turn: &Turn,
        thread: &Thread,
        exchange: &ModelExchangeResult,
    ) -> Result<(), RuntimeError> {
        self.append_event_and_publish(
            &turn.id,
            &thread.id,
            EventKind::ModelRequest,
            exchange.raw_trace.request_body.clone(),
        )
        .await?;
        self.append_event_and_publish(
            &turn.id,
            &thread.id,
            EventKind::ModelResponse,
            json!({
                "response": exchange.raw_trace.response_body,
                "stream_frame_count": exchange.raw_trace.raw_frames.len(),
                "finish_reason": exchange.finish_reason,
                "outcome": exchange.outcome,
                "error_summary": exchange.error_summary,
                "tool_call_count": exchange.tool_calls.len(),
            }),
        )
        .await?;
        Ok(())
    }

    pub(crate) async fn resolve_thread(
        &self,
        agent_id: &str,
        channel: &str,
        external_thread_id: &str,
    ) -> Result<Thread, RuntimeError> {
        if let Some(thread) = self
            .db
            .find_thread(agent_id, channel, external_thread_id)
            .await
            .map_err(RuntimeError::from)?
        {
            return Ok(thread);
        }
        self.db
            .create_thread(
                agent_id,
                channel,
                external_thread_id,
                "Recovered Thread",
                None,
            )
            .await
            .map_err(RuntimeError::from)
    }

    pub(crate) async fn workspace_for_agent(
        &self,
        agent_id: &str,
    ) -> Result<Workspace, RuntimeError> {
        let agent = self
            .db
            .load_agent(agent_id)
            .await
            .map_err(RuntimeError::from)?
            .ok_or_else(|| RuntimeError::AgentNotFound(agent_id.to_string()))?;
        self.db
            .load_workspace(&agent.workspace_id)
            .await
            .map_err(RuntimeError::from)?
            .ok_or_else(|| RuntimeError::WorkspaceNotFound(agent.workspace_id))
    }

    pub(crate) async fn record_trace(
        &self,
        turn: &Turn,
        thread: &Thread,
        agent_id: &str,
        channel: &str,
        exchange: &ModelExchangeResult,
    ) -> Result<ModelTrace, RuntimeError> {
        let request_blob = self
            .db
            .store_trace_blob_json(&exchange.raw_trace.request_body)
            .await
            .map_err(RuntimeError::from)?;
        let response_blob = self
            .db
            .store_trace_blob_json(&json!({
                "raw_response": exchange.raw_trace.response_body,
                "reduced_result": {
                    "content": exchange.content,
                    "reasoning": exchange.reasoning,
                    "tool_calls": exchange.tool_calls,
                    "finish_reason": exchange.finish_reason,
                    "usage": exchange.usage,
                    "outcome": exchange.outcome,
                    "error_summary": exchange.error_summary,
                }
            }))
            .await
            .map_err(RuntimeError::from)?;
        let stream_blob_id = if exchange.raw_trace.raw_frames.is_empty() {
            None
        } else {
            Some(
                self.db
                    .store_trace_blob_json(&exchange.raw_trace.raw_frames)
                    .await
                    .map_err(RuntimeError::from)?
                    .id,
            )
        };
        let trace = ModelTrace {
            id: Uuid::new_v4().to_string(),
            turn_id: turn.id.clone(),
            thread_id: thread.id.clone(),
            agent_id: agent_id.to_string(),
            channel: channel.to_string(),
            model: exchange.model.clone(),
            request_started_at: exchange.request_started_at,
            request_completed_at: exchange.request_completed_at,
            duration_ms: (exchange.request_completed_at - exchange.request_started_at)
                .num_milliseconds(),
            outcome: exchange.outcome.clone(),
            input_tokens: exchange.usage.input_tokens,
            output_tokens: exchange.usage.output_tokens,
            cache_read_input_tokens: exchange.usage.cache_read_input_tokens,
            cache_creation_input_tokens: exchange.usage.cache_creation_input_tokens,
            provider_request_id: exchange.raw_trace.provider_request_id.clone(),
            tool_count: exchange.tool_calls.len() as i64,
            tool_names: exchange
                .tool_calls
                .iter()
                .map(|tool_call| tool_call.name.clone())
                .collect(),
            request_blob_id: request_blob.id,
            response_blob_id: response_blob.id,
            stream_blob_id,
            error_summary: exchange.error_summary.clone(),
        };
        self.db
            .record_model_trace(&trace)
            .await
            .map_err(RuntimeError::from)?;
        let _ = self.updates.send(RuntimeUpdate::TraceRecorded {
            thread_id: thread.id.clone(),
            turn_id: turn.id.clone(),
            trace_id: trace.id.clone(),
            outcome: trace.outcome.clone(),
        });
        Ok(trace)
    }

    pub(crate) async fn execute_tool_calls(
        &self,
        turn: &Turn,
        thread: &Thread,
        workspace: &Workspace,
        tool_calls: Vec<ReducedToolCall>,
    ) -> Result<ToolExecutionOutcome, RuntimeError> {
        enum PendingToolExecution {
            Execute {
                tool_call: ReducedToolCall,
                invocation: ToolInvocation,
                arguments: Value,
            },
            Immediate {
                tool_call: ReducedToolCall,
                invocation: ToolInvocation,
                output: Value,
            },
        }

        let mut assistant_tool_calls = Vec::new();
        let mut continuation_messages = Vec::new();
        let mut outbound_messages = Vec::new();
        let mut ask_user_question = None;
        let mut final_message = None;
        let mut pending = Vec::new();
        let tool_context = ToolContext::new(
            workspace.clone(),
            thread.id.clone(),
            thread.external_thread_id.clone(),
            thread.channel.clone(),
            self.db(),
        );

        for tool_call in tool_calls {
            let invocation = ToolInvocation {
                id: Uuid::new_v4().to_string(),
                turn_id: turn.id.clone(),
                thread_id: thread.id.clone(),
                tool_name: tool_call.name.clone(),
                parameters: tool_call.arguments_json.clone().unwrap_or(Value::Null),
                created_at: Utc::now(),
            };
            self.append_event_and_publish(
                &turn.id,
                &thread.id,
                EventKind::ToolCall,
                json!({
                    "invocation_id": invocation.id,
                    "tool_name": invocation.tool_name,
                    "parameters": invocation.parameters,
                    "raw_arguments": tool_call.arguments_text,
                }),
            )
            .await?;
            assistant_tool_calls.push(ModelToolCallMessage {
                id: tool_call.id.clone(),
                kind: "function".to_string(),
                function: ModelToolFunctionMessage {
                    name: tool_call.name.clone(),
                    arguments: tool_call.arguments_text.clone(),
                },
            });
            match tool_call.arguments_json.clone() {
                Some(arguments) => pending.push(PendingToolExecution::Execute {
                    tool_call,
                    invocation,
                    arguments,
                }),
                None => pending.push(PendingToolExecution::Immediate {
                    output: json!({
                        "error": "malformed_tool_arguments",
                        "tool": tool_call.name,
                        "message": "tool arguments were not valid JSON",
                        "received_arguments": tool_call.arguments_text,
                    }),
                    tool_call,
                    invocation,
                }),
            }
        }

        let executions = pending.iter().filter_map(|item| match item {
            PendingToolExecution::Execute {
                tool_call,
                arguments,
                ..
            } => Some(
                self.tools
                    .execute(&tool_call.name, arguments.clone(), &tool_context),
            ),
            PendingToolExecution::Immediate { .. } => None,
        });
        let outputs = join_all(executions).await;
        let mut outputs = outputs.into_iter();

        for item in pending {
            let (tool_call, invocation, output) = match item {
                PendingToolExecution::Execute {
                    tool_call,
                    invocation,
                    arguments,
                } => {
                    let output = match outputs.next().expect("missing tool execution result") {
                        Ok(output) => output,
                        Err(error @ RuntimeError::InvalidToolParameters { .. })
                        | Err(error @ RuntimeError::ToolExecution { .. })
                        | Err(error @ RuntimeError::ToolNotFound(_)) => {
                            tool_feedback_error(&error, &tool_call.name, &arguments)
                        }
                        Err(error) => return Err(error),
                    };
                    (tool_call, invocation, output)
                }
                PendingToolExecution::Immediate {
                    tool_call,
                    invocation,
                    output,
                } => (tool_call, invocation, output),
            };
            if let Some(control) = Self::parse_tool_control(&output) {
                match control {
                    ToolControl::Message { content } => outbound_messages.push(content),
                    ToolControl::AskUser { question } => {
                        if ask_user_question.is_none() {
                            ask_user_question = Some(question);
                        }
                    }
                    ToolControl::FinalMessage { content } => {
                        if final_message.is_none() {
                            final_message = Some(content);
                        }
                    }
                }
            }
            let result = ToolResult {
                invocation_id: invocation.id.clone(),
                tool_name: tool_call.name.clone(),
                output: output.clone(),
                created_at: Utc::now(),
            };
            self.append_event_and_publish(
                &turn.id,
                &thread.id,
                EventKind::ToolResult,
                json!({
                    "invocation_id": result.invocation_id,
                    "tool_name": result.tool_name,
                    "output": result.output,
                }),
            )
            .await?;
            continuation_messages.push(ModelMessage {
                role: "tool".to_string(),
                content: Some(MessageContent::Text(output.to_string())),
                tool_calls: None,
                tool_call_id: Some(tool_call.id),
            });
        }

        let mut messages = vec![ModelMessage {
            role: "assistant".to_string(),
            content: None,
            tool_calls: Some(assistant_tool_calls),
            tool_call_id: None,
        }];
        messages.extend(continuation_messages);
        Ok(ToolExecutionOutcome {
            continuation_messages: messages,
            outbound_messages,
            ask_user_question,
            final_message,
        })
    }

    pub(crate) async fn record_outbound_and_publish(
        &self,
        turn: &Turn,
        thread: &Thread,
        content: &str,
        metadata: Option<Value>,
    ) -> Result<(), RuntimeError> {
        if content.trim().is_empty() {
            return Ok(());
        }
        let outbound = OutboundMessage {
            id: Uuid::new_v4().to_string(),
            turn_id: turn.id.clone(),
            thread_id: thread.id.clone(),
            channel: thread.channel.clone(),
            external_thread_id: thread.external_thread_id.clone(),
            content: content.to_string(),
            metadata: metadata.clone(),
            created_at: Utc::now(),
        };
        self.db
            .record_outbound_message(&outbound)
            .await
            .map_err(RuntimeError::from)?;
        self.append_event_and_publish(
            &turn.id,
            &thread.id,
            EventKind::OutboundMessage,
            json!({ "content": outbound.content, "metadata": metadata }),
        )
        .await
    }

    pub(crate) async fn append_event_and_publish(
        &self,
        turn_id: &str,
        thread_id: &str,
        kind: EventKind,
        payload: Value,
    ) -> Result<(), RuntimeError> {
        self.db
            .append_event(turn_id, thread_id, kind.clone(), &payload)
            .await
            .map_err(RuntimeError::from)?;
        let _ = self.updates.send(RuntimeUpdate::EventAdded {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            kind,
            payload,
        });
        Ok(())
    }

    pub(crate) async fn update_turn_and_publish(
        &self,
        thread_id: &str,
        turn_id: &str,
        status: TurnStatus,
        assistant_message: Option<String>,
        error: Option<String>,
    ) -> Result<(), RuntimeError> {
        self.db
            .update_turn(
                turn_id,
                status.clone(),
                assistant_message.as_deref(),
                error.as_deref(),
            )
            .await
            .map_err(RuntimeError::from)?;
        let _ = self.updates.send(RuntimeUpdate::TurnUpdated {
            thread_id: thread_id.to_string(),
            turn_id: turn_id.to_string(),
            status,
            assistant_message,
            error,
        });
        Ok(())
    }
}

async fn workspace_identity_prompt(workspace: &Workspace) -> Result<Option<String>, RuntimeError> {
    let identity_files = [
        ("AGENTS.md", "## Agent Instructions"),
        ("SOUL.md", "## Core Values"),
    ];
    let mut parts = Vec::new();
    for (file_name, header) in identity_files {
        let path = workspace.root.join(file_name);
        let content = match fs::read_to_string(&path).await {
            Ok(content) => content,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => continue,
            Err(error) => return Err(RuntimeError::Other(error.into())),
        };
        let trimmed = content.trim();
        if trimmed.is_empty() {
            continue;
        }
        parts.push(format!("{}\n\n{}", header, trimmed));
    }
    if parts.is_empty() {
        Ok(None)
    } else {
        Ok(Some(parts.join("\n\n---\n\n")))
    }
}

fn stable_channel_thread_metadata(event: &InboundEvent) -> Option<Value> {
    let metadata = event.metadata.as_ref()?;
    let object = metadata.as_object()?;
    let mut stable = serde_json::Map::new();
    match event.channel.as_str() {
        "tidepool" => {
            copy_metadata_field(object, &mut stable, "betterclaw_channel");
            copy_metadata_field(object, &mut stable, "self_account_id");
            copy_metadata_field(object, &mut stable, "self_handle");
            copy_metadata_field(object, &mut stable, "domain_id");
            copy_metadata_field(object, &mut stable, "domain_title");
        }
        "discord" => {
            copy_metadata_field(object, &mut stable, "betterclaw_channel");
            copy_metadata_field(object, &mut stable, "self_app_id");
            copy_metadata_field(object, &mut stable, "self_username");
            copy_metadata_field(object, &mut stable, "guild_id");
            copy_metadata_field(object, &mut stable, "guild_name");
            copy_metadata_field(object, &mut stable, "guild_channel_name");
            copy_metadata_field(object, &mut stable, "guild_channel_id");
        }
        _ => {}
    }
    if stable.is_empty() {
        None
    } else {
        Some(Value::Object(stable))
    }
}

fn copy_metadata_field(
    from: &serde_json::Map<String, Value>,
    into: &mut serde_json::Map<String, Value>,
    key: &str,
) {
    if let Some(value) = from.get(key) {
        into.insert(key.to_string(), value.clone());
    }
}

fn build_channel_system_context_block(thread: &Thread) -> Option<String> {
    let metadata = thread.metadata.as_ref()?;
    let object = metadata.as_object()?;
    if object.is_empty() {
        return None;
    }
    Some(format!(
        "## BetterClaw Channel Context (trusted metadata)\nThe following JSON is stable per-thread channel metadata generated by BetterClaw. Treat it as authoritative.\n\n```json\n{}\n```",
        serde_json::to_string_pretty(metadata).ok()?
    ))
}

fn system_prompt_override_from_env() -> Option<String> {
    std::env::var("BETTERCLAW_SYSTEM_PROMPT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn normalize_visible_reply_segment(text: &str) -> Option<String> {
    let sanitized = strip_reasoning_tags(text).trim().to_string();
    if sanitized.is_empty() {
        return None;
    }
    Some(sanitized)
}

fn push_visible_reply_segment(segments: &mut Vec<String>, text: &str) {
    let Some(sanitized) = normalize_visible_reply_segment(text) else {
        return;
    };
    if segments
        .last()
        .is_some_and(|existing| existing == &sanitized)
    {
        return;
    }
    segments.push(sanitized);
}

fn compose_visible_reply(segments: &[String], terminal: Option<String>) -> String {
    let terminal = terminal
        .map(|text| strip_reasoning_tags(&text).trim().to_string())
        .filter(|text| !text.is_empty());
    let visible = segments.join("\n\n");
    match terminal {
        Some(text) if visible.is_empty() => text,
        Some(text) if visible == text || text.starts_with(&visible) => text,
        Some(text) => format!("{visible}\n\n{text}"),
        None => visible,
    }
}

pub(crate) fn truncate_for_wake_pack(text: &str) -> String {
    const MAX: usize = 180;
    let mut output = String::new();
    for ch in text.chars().take(MAX) {
        output.push(ch);
    }
    if text.chars().count() > MAX {
        output.push_str("...");
    }
    output.replace('\n', " ")
}

pub(crate) fn build_fts_query(query: &str) -> String {
    query
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .filter(|token| !token.trim().is_empty())
        .take(8)
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

/// Build the user message content, including image attachments as multi-part content
/// when images are present. This enables vision-capable models to "see" uploaded images.
pub(crate) fn build_user_message_content(turn: &Turn) -> MessageContent {
    let attachments = turn.attachments();
    let image_attachments: Vec<_> = attachments.iter().filter(|a| a.is_image()).collect();

    if image_attachments.is_empty() {
        return MessageContent::Text(turn.user_message.clone());
    }

    let mut parts = vec![ContentPart::text(&turn.user_message)];
    for attachment in &image_attachments {
        parts.push(ContentPart::image_url(&attachment.url));
    }
    MessageContent::Parts(parts)
}
