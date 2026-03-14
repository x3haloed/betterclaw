use std::path::PathBuf;
use std::sync::Arc;

use chrono::Utc;
use serde_json::json;
use uuid::Uuid;

use crate::agent::Agent;
use crate::channel::{InboundEvent, OutboundMessage};
use crate::db::Db;
use crate::error::RuntimeError;
use crate::event::EventKind;
use crate::model::{
    ModelExchange, ModelMessage, ModelRequest, ModelResponse, ModelTrace, TraceOutcome,
};
use crate::thread::Thread;
use crate::tool::{ToolCall, ToolInvocation, ToolRegistry, ToolResult};
use crate::turn::{Turn, TurnStatus};
use crate::workspace::Workspace;

#[derive(Clone)]
pub struct Runtime {
    db: Arc<Db>,
    tools: ToolRegistry,
    model_name: String,
}

impl Runtime {
    pub async fn new(db: Db) -> Result<Self, RuntimeError> {
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
            model_name: "local-debug-model".to_string(),
        })
    }

    pub fn db(&self) -> Arc<Db> {
        Arc::clone(&self.db)
    }

    pub fn tool_definitions(&self) -> Vec<serde_json::Value> {
        self.tools
            .definitions()
            .into_iter()
            .map(|tool| {
                json!({
                    "name": tool.name,
                    "description": tool.description,
                    "parameters_schema": tool.parameters_schema
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
    ) -> Result<Option<crate::model::TraceDetail>, RuntimeError> {
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

        let model_request = ModelRequest {
            model: self.model_name.clone(),
            messages: vec![ModelMessage {
                role: "user".to_string(),
                content: turn.user_message.clone(),
            }],
            tools: self.tool_definitions(),
        };
        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::ContextAssembled,
                &json!({ "message_count": model_request.messages.len(), "tool_count": model_request.tools.len() }),
            )
            .await
            .map_err(RuntimeError::from)?;

        let exchange = self.invoke_model(&model_request);
        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::ModelRequest,
                &exchange.raw_request,
            )
            .await
            .map_err(RuntimeError::from)?;
        self.db
            .append_event(
                &turn.id,
                &thread.id,
                EventKind::ModelResponse,
                &exchange.raw_response,
            )
            .await
            .map_err(RuntimeError::from)?;

        let trace = self
            .record_trace(&turn, &thread, &event.agent_id, &event.channel, &exchange)
            .await?;

        match exchange.parsed {
            Ok(response) => {
                let final_response = if response.tool_calls.is_empty() {
                    response.content.unwrap_or_default()
                } else {
                    self.execute_tool_calls(&turn, &thread, &workspace, response.tool_calls)
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
            Err(error) => {
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
                Err(RuntimeError::ModelParse(error))
            }
        }
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

    fn invoke_model(&self, request: &ModelRequest) -> ModelExchange {
        let request_started_at = Utc::now();
        let last_message = request
            .messages
            .last()
            .map(|message| message.content.clone())
            .unwrap_or_default();
        let raw_request = json!({
            "model": request.model,
            "messages": request.messages,
            "tools": request.tools,
        });

        let parsed = if let Some(rest) = last_message.strip_prefix("/tool ") {
            let mut parts = rest.splitn(2, ' ');
            let tool_name = parts.next().unwrap_or_default();
            let args = parts.next().unwrap_or("{}");
            match serde_json::from_str(args) {
                Ok(arguments) => Ok(ModelResponse {
                    content: None,
                    tool_calls: vec![ToolCall {
                        id: Uuid::new_v4().to_string(),
                        name: tool_name.to_string(),
                        arguments,
                    }],
                }),
                Err(error) => Err(format!(
                    "Tool call '{tool_name}' had malformed JSON arguments: {error}"
                )),
            }
        } else if let Some(rest) = last_message.strip_prefix("/malformed-tool ") {
            let mut parts = rest.splitn(2, ' ');
            let tool_name = parts.next().unwrap_or_default();
            let args = parts.next().unwrap_or("{");
            Err(format!(
                "Tool call '{tool_name}' had malformed JSON arguments: simulated malformed input: {args}"
            ))
        } else {
            Ok(ModelResponse {
                content: Some(format!("Echo: {}", last_message)),
                tool_calls: Vec::new(),
            })
        };

        let raw_response = match &parsed {
            Ok(response) => {
                if response.tool_calls.is_empty() {
                    json!({ "content": response.content })
                } else {
                    json!({
                        "tool_calls": response.tool_calls.iter().map(|tool_call| {
                            json!({
                                "id": tool_call.id,
                                "name": tool_call.name,
                                "arguments": tool_call.arguments,
                            })
                        }).collect::<Vec<_>>()
                    })
                }
            }
            Err(error) => json!({
                "error": error,
                "tool_calls": [{
                    "id": Uuid::new_v4().to_string(),
                    "name": "malformed",
                    "arguments": "{"
                }]
            }),
        };

        let request_completed_at = Utc::now();
        ModelExchange {
            request_started_at,
            request_completed_at,
            raw_request,
            raw_response,
            parsed,
            provider_request_id: Some(Uuid::new_v4().to_string()),
            input_tokens: last_message.len() as i64,
            output_tokens: 32,
        }
    }

    async fn record_trace(
        &self,
        turn: &Turn,
        thread: &Thread,
        agent_id: &str,
        channel: &str,
        exchange: &ModelExchange,
    ) -> Result<ModelTrace, RuntimeError> {
        let request_blob = self
            .db
            .store_trace_blob(&exchange.raw_request)
            .await
            .map_err(RuntimeError::from)?;
        let response_blob = self
            .db
            .store_trace_blob(&exchange.raw_response)
            .await
            .map_err(RuntimeError::from)?;
        let trace = ModelTrace {
            id: Uuid::new_v4().to_string(),
            turn_id: turn.id.clone(),
            thread_id: thread.id.clone(),
            agent_id: agent_id.to_string(),
            channel: channel.to_string(),
            model: self.model_name.clone(),
            request_started_at: exchange.request_started_at,
            request_completed_at: exchange.request_completed_at,
            duration_ms: (exchange.request_completed_at - exchange.request_started_at)
                .num_milliseconds(),
            outcome: if exchange.parsed.is_ok() {
                TraceOutcome::Ok
            } else {
                TraceOutcome::ParseError
            },
            input_tokens: exchange.input_tokens,
            output_tokens: exchange.output_tokens,
            cache_read_input_tokens: 0,
            cache_creation_input_tokens: 0,
            provider_request_id: exchange.provider_request_id.clone(),
            tool_count: exchange
                .parsed
                .as_ref()
                .map(|response| response.tool_calls.len() as i64)
                .unwrap_or(0),
            tool_names: exchange
                .parsed
                .as_ref()
                .map(|response| {
                    response
                        .tool_calls
                        .iter()
                        .map(|tool_call| tool_call.name.clone())
                        .collect()
                })
                .unwrap_or_default(),
            request_blob_id: request_blob.id,
            response_blob_id: response_blob.id,
            stream_blob_id: None,
            error_summary: exchange.parsed.as_ref().err().cloned(),
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
        tool_calls: Vec<ToolCall>,
    ) -> Result<String, RuntimeError> {
        let mut parts = Vec::new();
        for tool_call in tool_calls {
            let invocation = ToolInvocation {
                id: Uuid::new_v4().to_string(),
                turn_id: turn.id.clone(),
                thread_id: thread.id.clone(),
                tool_name: tool_call.name.clone(),
                parameters: tool_call.arguments.clone(),
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
                    }),
                )
                .await
                .map_err(RuntimeError::from)?;
            let output = self
                .tools
                .execute(&tool_call.name, tool_call.arguments.clone(), workspace)
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
