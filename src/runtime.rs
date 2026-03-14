use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use futures_util::future::join_all;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::agent::Agent;
use crate::channel::{InboundEvent, OutboundMessage};
use crate::db::Db;
use crate::error::RuntimeError;
use crate::event::EventKind;
use crate::model::{
    ModelEngine, ModelExchangeRequest, ModelExchangeResult, ModelMessage, ModelToolCallMessage,
    ModelToolFunctionMessage, ModelTrace, OpenAiChatCompletionsConfig, OpenAiChatCompletionsEngine,
    ReducedToolCall, StubModelEngine, TraceDetail, TraceOutcome, strip_reasoning_tags,
};
use crate::settings::RuntimeSettings;
use crate::thread::Thread;
use crate::tool::{ToolInvocation, ToolRegistry, ToolResult};
use crate::turn::{Turn, TurnStatus};
use crate::workspace::Workspace;

#[derive(Clone)]
pub struct Runtime {
    db: Arc<Db>,
    tools: ToolRegistry,
    model_engine: Arc<ModelEngine>,
    updates: broadcast::Sender<RuntimeUpdate>,
}

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeUpdate {
    EventAdded {
        thread_id: String,
        turn_id: String,
        kind: EventKind,
        payload: Value,
    },
    TraceRecorded {
        thread_id: String,
        turn_id: String,
        trace_id: String,
        outcome: TraceOutcome,
    },
    TurnUpdated {
        thread_id: String,
        turn_id: String,
        status: TurnStatus,
        assistant_message: Option<String>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecoveryReport {
    pub recovered_turn_count: usize,
    pub recovered_turn_ids: Vec<String>,
}

impl Runtime {
    pub async fn new(db: Db) -> Result<Self, RuntimeError> {
        Self::with_model_engine(db, ModelEngine::stub(StubModelEngine), "local-debug-model").await
    }

    pub async fn from_env(db: Db) -> Result<Self, RuntimeError> {
        let model_name =
            std::env::var("BETTERCLAW_MODEL").unwrap_or_else(|_| "qwen/qwen3.5-9b".to_string());
        let base_url = std::env::var("BETTERCLAW_MODEL_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:1234/v1".to_string());
        let engine = OpenAiChatCompletionsEngine::new(OpenAiChatCompletionsConfig {
            base_url,
            ..OpenAiChatCompletionsConfig::default()
        })?;
        Self::with_model_engine(db, ModelEngine::openai_chat_completions(engine), model_name).await
    }

    pub async fn with_model_engine(
        db: Db,
        model_engine: ModelEngine,
        model_name: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let db = Arc::new(db);
        let (updates, _) = broadcast::channel(512);
        let workspace = Workspace::new(
            "default",
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        );
        let agent = Agent::new("default", "Default Agent", workspace.id.clone());
        let default_settings = RuntimeSettings::with_defaults("default", model_name.into());
        db.seed_default_agent(&agent, &workspace)
            .await
            .map_err(RuntimeError::from)?;
        db.seed_runtime_settings(&default_settings)
            .await
            .map_err(RuntimeError::from)?;

        let runtime = Self {
            db,
            tools: ToolRegistry::with_defaults(),
            model_engine: Arc::new(model_engine),
            updates,
        };
        runtime.recover_incomplete_turns().await?;
        Ok(runtime)
    }

    pub fn db(&self) -> Arc<Db> {
        Arc::clone(&self.db)
    }

    pub async fn get_runtime_settings(
        &self,
        agent_id: &str,
    ) -> Result<RuntimeSettings, RuntimeError> {
        self.db
            .load_runtime_settings(agent_id)
            .await
            .map_err(RuntimeError::from)?
            .ok_or_else(|| RuntimeError::AgentNotFound(agent_id.to_string()))
    }

    pub async fn update_runtime_settings(
        &self,
        mut settings: RuntimeSettings,
    ) -> Result<RuntimeSettings, RuntimeError> {
        let existing = self
            .db
            .load_runtime_settings(&settings.agent_id)
            .await
            .map_err(RuntimeError::from)?;
        let created_at = existing
            .as_ref()
            .map(|item| item.created_at)
            .unwrap_or(settings.created_at);
        settings.created_at = created_at;
        settings.updated_at = Utc::now();
        self.db
            .update_runtime_settings(&settings)
            .await
            .map_err(RuntimeError::from)?;
        Ok(settings)
    }

    pub fn subscribe_updates(&self) -> broadcast::Receiver<RuntimeUpdate> {
        self.updates.subscribe()
    }

    pub fn tool_definitions(&self) -> Vec<Value> {
        self.tools
            .definitions()
            .into_iter()
            .map(|tool| {
                json!({
                    "type": "function",
                    "function": {
                        "name": tool.name,
                        "description": tool.description,
                        "parameters": tool.parameters_schema
                    }
                })
            })
            .collect()
    }

    pub async fn create_web_thread(&self, title: Option<String>) -> Result<Thread, RuntimeError> {
        let external_thread_id = Uuid::new_v4().to_string();
        let title = title.unwrap_or_else(|| "New Thread".to_string());
        self.db
            .create_thread("default", "web", &external_thread_id, &title)
            .await
            .map_err(RuntimeError::from)
    }

    pub async fn list_threads(&self) -> Result<Vec<Thread>, RuntimeError> {
        self.db.list_threads().await.map_err(RuntimeError::from)
    }

    pub async fn get_thread(&self, thread_id: &str) -> Result<Option<Thread>, RuntimeError> {
        self.db
            .get_thread(thread_id)
            .await
            .map_err(RuntimeError::from)
    }

    pub async fn list_thread_timeline(
        &self,
        thread_id: &str,
    ) -> Result<Vec<crate::event::Event>, RuntimeError> {
        self.db
            .list_thread_events(thread_id)
            .await
            .map_err(RuntimeError::from)
    }

    pub async fn list_thread_turns(&self, thread_id: &str) -> Result<Vec<Turn>, RuntimeError> {
        self.db
            .list_thread_turns(thread_id)
            .await
            .map_err(RuntimeError::from)
    }

    pub async fn list_turn_traces(&self, turn_id: &str) -> Result<Vec<ModelTrace>, RuntimeError> {
        self.db
            .list_turn_traces(turn_id)
            .await
            .map_err(RuntimeError::from)
    }

    pub async fn get_trace_detail(
        &self,
        trace_id: &str,
    ) -> Result<Option<TraceDetail>, RuntimeError> {
        self.db
            .get_trace_detail(trace_id)
            .await
            .map_err(RuntimeError::from)
    }

    pub async fn get_turn(&self, turn_id: &str) -> Result<Option<Turn>, RuntimeError> {
        self.db.get_turn(turn_id).await.map_err(RuntimeError::from)
    }

    pub async fn recover_incomplete_turns(&self) -> Result<RecoveryReport, RuntimeError> {
        let running_turns = self
            .db
            .list_running_turns()
            .await
            .map_err(RuntimeError::from)?;
        let mut recovered_turn_ids = Vec::new();
        for turn in running_turns {
            let message = "Recovered abandoned running turn during runtime startup".to_string();
            self.update_turn_and_publish(
                &turn.thread_id,
                &turn.id,
                TurnStatus::Failed,
                None,
                Some(message.clone()),
            )
            .await?;
            self.append_event_and_publish(
                &turn.id,
                &turn.thread_id,
                EventKind::TurnRecovered,
                json!({
                    "recovered_at": Utc::now(),
                    "reason": message,
                }),
            )
            .await?;
            recovered_turn_ids.push(turn.id);
        }
        Ok(RecoveryReport {
            recovered_turn_count: recovered_turn_ids.len(),
            recovered_turn_ids,
        })
    }

    pub async fn replay_turn(&self, source_turn_id: &str) -> Result<TurnOutcome, RuntimeError> {
        let source_turn = self
            .get_turn(source_turn_id)
            .await?
            .ok_or_else(|| RuntimeError::TurnNotFound(source_turn_id.to_string()))?;
        let thread = self
            .get_thread(&source_turn.thread_id)
            .await?
            .ok_or_else(|| RuntimeError::ThreadNotFound(source_turn.thread_id.clone()))?;
        self.handle_inbound_internal(
            InboundEvent {
                agent_id: thread.agent_id.clone(),
                channel: thread.channel.clone(),
                external_thread_id: thread.external_thread_id.clone(),
                content: source_turn.user_message,
                received_at: Utc::now(),
            },
            Some(source_turn_id.to_string()),
        )
        .await
    }

    pub async fn handle_inbound(&self, event: InboundEvent) -> Result<TurnOutcome, RuntimeError> {
        self.handle_inbound_internal(event, None).await
    }

    async fn handle_inbound_internal(
        &self,
        event: InboundEvent,
        replay_source_turn_id: Option<String>,
    ) -> Result<TurnOutcome, RuntimeError> {
        let thread = self
            .resolve_thread(&event.agent_id, &event.channel, &event.external_thread_id)
            .await?;
        let workspace = self.workspace_for_agent(&event.agent_id).await?;
        let settings = self.get_runtime_settings(&event.agent_id).await?;
        let turn = self
            .db
            .create_turn(&thread.id, &event.content)
            .await
            .map_err(RuntimeError::from)?;

        self.append_event_and_publish(
            &turn.id,
            &thread.id,
            EventKind::InboundMessage,
            json!({ "content": event.content, "received_at": event.received_at }),
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

        let mut conversation = self
            .build_conversation_history(&thread, &turn, &settings)
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
                "stream": initial_request.stream,
            }),
        )
        .await?;

        let mut request = initial_request;
        let (final_response, last_trace_id) = loop {
            let exchange = self
                .run_and_record_exchange(&turn, &thread, &event.agent_id, &event.channel, request)
                .await?;
            let trace = self
                .record_trace(&turn, &thread, &event.agent_id, &event.channel, &exchange)
                .await?;
            let trace_id = trace.id;

            if exchange.outcome != TraceOutcome::Ok {
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
                break (
                    exchange
                        .content
                        .as_deref()
                        .map(strip_reasoning_tags)
                        .unwrap_or_default(),
                    trace_id,
                );
            }

            let continuation_messages = match self
                .execute_tool_calls(&turn, &thread, &workspace, exchange.tool_calls)
                .await
            {
                Ok(messages) => messages,
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
            conversation.extend(continuation_messages);
            request = self.build_model_request(conversation.clone(), true, &settings);
        };

        self.update_turn_and_publish(
            &thread.id,
            &turn.id,
            TurnStatus::Succeeded,
            Some(final_response.clone()),
            None,
        )
        .await?;
        let outbound = OutboundMessage {
            id: Uuid::new_v4().to_string(),
            turn_id: turn.id.clone(),
            thread_id: thread.id.clone(),
            channel: thread.channel.clone(),
            external_thread_id: thread.external_thread_id.clone(),
            content: final_response.clone(),
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
            json!({ "content": outbound.content }),
        )
        .await?;

        Ok(TurnOutcome {
            thread,
            turn_id: turn.id,
            response: final_response,
            trace_id: last_trace_id,
        })
    }

    async fn build_conversation_history(
        &self,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<Vec<ModelMessage>, RuntimeError> {
        let mut messages = Vec::new();
        if !settings.system_prompt.trim().is_empty() {
            messages.push(ModelMessage {
                role: "system".to_string(),
                content: Some(settings.system_prompt.clone()),
                tool_calls: None,
                tool_call_id: None,
            });
        }
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
                content: Some(prior_turn.user_message.clone()),
                tool_calls: None,
                tool_call_id: None,
            });
            if let Some(assistant_message) = prior_turn.assistant_message.clone() {
                messages.push(ModelMessage {
                    role: "assistant".to_string(),
                    content: Some(strip_reasoning_tags(&assistant_message)),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        messages.push(ModelMessage {
            role: "user".to_string(),
            content: Some(turn.user_message.clone()),
            tool_calls: None,
            tool_call_id: None,
        });
        Ok(messages)
    }

    fn build_model_request(
        &self,
        messages: Vec<ModelMessage>,
        allow_tools: bool,
        settings: &RuntimeSettings,
    ) -> ModelExchangeRequest {
        ModelExchangeRequest {
            model: settings.model.clone(),
            messages,
            tools: if allow_tools && settings.allow_tools {
                self.tool_definitions()
            } else {
                Vec::new()
            },
            temperature: Some(settings.temperature),
            max_tokens: Some(settings.max_tokens),
            stream: settings.stream,
            response_format: None,
            extra: json!({}),
        }
    }

    async fn run_and_record_exchange(
        &self,
        turn: &Turn,
        thread: &Thread,
        agent_id: &str,
        channel: &str,
        request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, RuntimeError> {
        match self.model_engine.run(request).await {
            Ok(exchange) => {
                self.append_model_events(turn, thread, &exchange).await?;
                Ok(exchange)
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
                Err(RuntimeError::ModelEngine(error))
            }
        }
    }

    async fn append_model_events(
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

    async fn resolve_thread(
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
            .create_thread(agent_id, channel, external_thread_id, "Recovered Thread")
            .await
            .map_err(RuntimeError::from)
    }

    async fn workspace_for_agent(&self, agent_id: &str) -> Result<Workspace, RuntimeError> {
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

    async fn record_trace(
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

    async fn execute_tool_calls(
        &self,
        turn: &Turn,
        thread: &Thread,
        workspace: &Workspace,
        tool_calls: Vec<ReducedToolCall>,
    ) -> Result<Vec<ModelMessage>, RuntimeError> {
        let mut assistant_tool_calls = Vec::new();
        let mut continuation_messages = Vec::new();
        let mut batch = Vec::new();

        for tool_call in tool_calls {
            let arguments = tool_call.arguments_json.clone().ok_or_else(|| {
                RuntimeError::ModelParse(format!(
                    "tool call '{}' had malformed JSON arguments: {}",
                    tool_call.name, tool_call.arguments_text
                ))
            })?;
            let invocation = ToolInvocation {
                id: Uuid::new_v4().to_string(),
                turn_id: turn.id.clone(),
                thread_id: thread.id.clone(),
                tool_name: tool_call.name.clone(),
                parameters: arguments.clone(),
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
            batch.push((tool_call, invocation, arguments));
        }

        let executions = batch.iter().map(|(tool_call, _, arguments)| {
            self.tools
                .execute(&tool_call.name, arguments.clone(), workspace)
        });
        let outputs = join_all(executions).await;

        for ((tool_call, invocation, _arguments), output) in batch.into_iter().zip(outputs) {
            let output = output?;
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
                content: Some(output.to_string()),
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
        Ok(messages)
    }

    async fn append_event_and_publish(
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

    async fn update_turn_and_publish(
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

pub struct TurnOutcome {
    pub thread: Thread,
    pub turn_id: String,
    pub response: String,
    pub trace_id: String,
}

#[cfg(test)]
mod tests {
    use tempfile::tempdir;

    use super::Runtime;
    use crate::channel::InboundEvent;
    use crate::db::Db;
    use crate::event::EventKind;
    use crate::model::strip_reasoning_tags;
    use crate::turn::TurnStatus;

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
    async fn assistant_messages_saved_without_reasoning_tags() {
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
            .create_turn(&thread.id, "stuck message")
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
}
