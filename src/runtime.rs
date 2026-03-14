use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde_json::{Value, json};
use uuid::Uuid;

use crate::agent::Agent;
use crate::channel::{InboundEvent, OutboundMessage};
use crate::db::Db;
use crate::error::RuntimeError;
use crate::event::EventKind;
use crate::model::{
    ModelEngine, ModelExchangeRequest, ModelMessage, ModelTrace, OpenAiChatCompletionsConfig,
    OpenAiChatCompletionsEngine, StubModelEngine, TraceDetail, TraceOutcome,
};
use crate::thread::Thread;
use crate::tool::{ToolInvocation, ToolRegistry, ToolResult};
use crate::turn::{Turn, TurnStatus};
use crate::workspace::Workspace;

#[derive(Clone)]
pub struct Runtime {
    db: Arc<Db>,
    tools: ToolRegistry,
    model_engine: Arc<ModelEngine>,
    model_name: String,
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
        Self::with_model_engine(
            db,
            ModelEngine::openai_chat_completions(engine),
            model_name,
        )
        .await
    }

    pub async fn with_model_engine(
        db: Db,
        model_engine: ModelEngine,
        model_name: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        let db = Arc::new(db);
        let workspace = Workspace::new(
            "default",
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        );
        let agent = Agent::new("default", "Default Agent", workspace.id.clone());
        db.seed_default_agent(&agent, &workspace)
            .await
            .map_err(RuntimeError::from)?;

        Ok(Self {
            db,
            tools: ToolRegistry::with_defaults(),
            model_engine: Arc::new(model_engine),
            model_name: model_name.into(),
        })
    }

    pub fn db(&self) -> Arc<Db> {
        Arc::clone(&self.db)
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

    pub async fn handle_inbound(&self, event: InboundEvent) -> Result<TurnOutcome, RuntimeError> {
        let thread = self
            .resolve_thread(&event.agent_id, &event.channel, &event.external_thread_id)
            .await?;
        let workspace = self.workspace_for_agent(&event.agent_id).await?;
        let turn = self
            .db
            .create_turn(&thread.id, &event.content)
            .await
            .map_err(RuntimeError::from)?;

        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::InboundMessage,
                &json!({ "content": event.content, "received_at": event.received_at }),
            )
            .await
            .map_err(RuntimeError::from)?;
        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::ThreadResolved,
                &json!({ "thread_id": thread.id, "external_thread_id": thread.external_thread_id }),
            )
            .await
            .map_err(RuntimeError::from)?;

        let model_request = self.build_model_request(&thread, &turn).await?;
        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::ContextAssembled,
                &json!({
                    "message_count": model_request.messages.len(),
                    "tool_count": model_request.tools.len(),
                    "model": model_request.model,
                    "stream": model_request.stream,
                }),
            )
            .await
            .map_err(RuntimeError::from)?;

        let exchange = match self.model_engine.run(model_request).await {
            Ok(exchange) => exchange,
            Err(error) => {
                self.record_trace(&turn, &thread, &event.agent_id, &event.channel, error.exchange())
                    .await?;
                self.append_model_events(&turn, &thread, error.exchange()).await?;
                self.db
                    .update_turn(
                        &turn.id,
                        TurnStatus::Failed,
                        None,
                        error.exchange().error_summary.as_deref(),
                    )
                    .await
                    .map_err(RuntimeError::from)?;
                self.db
                    .append_event(
                        &turn.id,
                        &thread.id,
                        EventKind::Error,
                        &json!({ "message": error.to_string() }),
                    )
                    .await
                    .map_err(RuntimeError::from)?;
                return Err(RuntimeError::ModelEngine(error));
            }
        };

        self.append_model_events(&turn, &thread, &exchange).await?;
        let trace = self
            .record_trace(&turn, &thread, &event.agent_id, &event.channel, &exchange)
            .await?;

        if exchange.outcome != TraceOutcome::Ok {
            let error = exchange
                .error_summary
                .clone()
                .unwrap_or_else(|| "model exchange failed".to_string());
            self.db
                .update_turn(&turn.id, TurnStatus::Failed, None, Some(&error))
                .await
                .map_err(RuntimeError::from)?;
            self.db
                .append_event(
                    &turn.id,
                    &thread.id,
                    EventKind::Error,
                    &json!({ "message": error }),
                )
                .await
                .map_err(RuntimeError::from)?;
            return Err(RuntimeError::ModelParse(
                exchange
                    .error_summary
                    .unwrap_or_else(|| "model exchange failed".to_string()),
            ));
        }

        let final_response = if exchange.tool_calls.is_empty() {
            exchange.content.unwrap_or_default()
        } else {
            self.execute_tool_calls(&turn, &thread, &workspace, exchange.tool_calls)
                .await?
        };
        self.db
            .update_turn(&turn.id, TurnStatus::Succeeded, Some(&final_response), None)
            .await
            .map_err(RuntimeError::from)?;
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
        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::OutboundMessage,
                &json!({ "content": outbound.content }),
            )
            .await
            .map_err(RuntimeError::from)?;

        Ok(TurnOutcome {
            thread,
            turn_id: turn.id,
            response: final_response,
            trace_id: trace.id,
        })
    }

    async fn build_model_request(
        &self,
        thread: &Thread,
        turn: &Turn,
    ) -> Result<ModelExchangeRequest, RuntimeError> {
        let mut messages = Vec::new();
        for prior_turn in self.list_thread_turns(&thread.id).await? {
            if prior_turn.id == turn.id {
                continue;
            }
            messages.push(ModelMessage {
                role: "user".to_string(),
                content: prior_turn.user_message,
            });
            if let Some(assistant_message) = prior_turn.assistant_message {
                messages.push(ModelMessage {
                    role: "assistant".to_string(),
                    content: assistant_message,
                });
            }
        }
        messages.push(ModelMessage {
            role: "user".to_string(),
            content: turn.user_message.clone(),
        });
        Ok(ModelExchangeRequest {
            model: self.model_name.clone(),
            messages,
            tools: self.tool_definitions(),
            temperature: Some(0.2),
            max_tokens: Some(1024),
            stream: true,
            response_format: None,
            extra: json!({}),
        })
    }

    async fn append_model_events(
        &self,
        turn: &Turn,
        thread: &Thread,
        exchange: &crate::model::ModelExchangeResult,
    ) -> Result<(), RuntimeError> {
        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::ModelRequest,
                &exchange.raw_trace.request_body,
            )
            .await
            .map_err(RuntimeError::from)?;
        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::ModelResponse,
                &json!({
                    "response": exchange.raw_trace.response_body,
                    "stream_frame_count": exchange.raw_trace.raw_frames.len(),
                    "finish_reason": exchange.finish_reason,
                    "outcome": exchange.outcome,
                    "error_summary": exchange.error_summary,
                }),
            )
            .await
            .map_err(RuntimeError::from)?;
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
        exchange: &crate::model::ModelExchangeResult,
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
        Ok(trace)
    }

    async fn execute_tool_calls(
        &self,
        turn: &Turn,
        thread: &Thread,
        workspace: &Workspace,
        tool_calls: Vec<crate::model::ReducedToolCall>,
    ) -> Result<String, RuntimeError> {
        let mut parts = Vec::new();
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
            self.db
                .append_event(
                    &turn.id,
                    &thread.id,
                    EventKind::ToolCall,
                    &json!({
                        "invocation_id": invocation.id,
                        "tool_name": invocation.tool_name,
                        "parameters": invocation.parameters,
                        "raw_arguments": tool_call.arguments_text,
                    }),
                )
                .await
                .map_err(RuntimeError::from)?;
            let output = self
                .tools
                .execute(&tool_call.name, arguments, workspace)
                .await?;
            let result = ToolResult {
                invocation_id: invocation.id.clone(),
                tool_name: tool_call.name.clone(),
                output: output.clone(),
                created_at: Utc::now(),
            };
            self.db
                .append_event(
                    &turn.id,
                    &thread.id,
                    EventKind::ToolResult,
                    &json!({
                        "invocation_id": result.invocation_id,
                        "tool_name": result.tool_name,
                        "output": result.output,
                    }),
                )
                .await
                .map_err(RuntimeError::from)?;
            parts.push(format!("{} => {}", tool_call.name, output));
        }
        Ok(parts.join("\n"))
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
}
