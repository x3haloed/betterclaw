use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use chrono::Utc;
use serde_json::{Value, json};
use tokio::sync::broadcast;
use uuid::Uuid;

use crate::agent::Agent;
use crate::channel::{InboundEvent, OutboundMessage};
use crate::db::Db;
use crate::error::RuntimeError;
use crate::event::EventKind;
use crate::memory::{MemoryArtifactKind, chunk_text, cosine_similarity};
use crate::model::{
    ModelEngine, ModelMessage, ModelTrace, StubModelEngine, TraceDetail, TraceOutcome,
    strip_reasoning_tags,
};
use crate::settings::{ModelRole, RetentionSettings, RuntimeSettings};
use crate::thread::Thread;
use crate::tool::{ToolRegistry, normalize_tool_parameters_schema};
use crate::turn::{Turn, TurnStatus};
use crate::workspace::Workspace;

fn default_workspace_root() -> PathBuf {
    dirs::home_dir()
        .map(|path| path.join(".betterclaw").join("workspaces").join("default").join("files"))
        .unwrap_or_else(|| PathBuf::from("."))
}

mod engine;
mod internal;
mod memory;
mod throttle;
mod types;

use engine::*;
use throttle::*;
pub use types::*;

#[cfg(test)]
mod tests;

#[derive(Clone)]
pub struct Runtime {
    pub(crate) db: Arc<Db>,
    pub(crate) tools: ToolRegistry,
    pub(crate) model_engine: Arc<ModelEngine>,
    pub(crate) provider_name: String,
    pub(crate) provider_throttle: Arc<ProviderThrottle>,
    pub(crate) provider_request_gate: Arc<tokio::sync::Mutex<()>>,
    pub(crate) updates: broadcast::Sender<RuntimeUpdate>,
}

impl Runtime {
    pub async fn new(db: Db) -> Result<Self, RuntimeError> {
        Self::with_model_engine_and_backoff(
            db,
            ModelEngine::stub(StubModelEngine::default()),
            "local-debug-model",
            "stub",
            Duration::from_secs(1),
        )
        .await
    }

    pub async fn from_env(db: Db) -> Result<Self, RuntimeError> {
        let resolved = ProviderPreset::from_env()?;
        Self::with_model_engine_and_backoff(
            db,
            resolved.engine,
            resolved.model_name,
            resolved.provider_name,
            Duration::from_secs(1),
        )
        .await
    }

    pub async fn with_model_engine(
        db: Db,
        model_engine: ModelEngine,
        model_name: impl Into<String>,
    ) -> Result<Self, RuntimeError> {
        Self::with_model_engine_and_backoff(
            db,
            model_engine,
            model_name,
            "custom",
            Duration::from_secs(1),
        )
        .await
    }

    async fn with_model_engine_and_backoff(
        db: Db,
        model_engine: ModelEngine,
        model_name: impl Into<String>,
        provider_name: impl Into<String>,
        base_backoff: Duration,
    ) -> Result<Self, RuntimeError> {
        let db = Arc::new(db);
        let (updates, _) = broadcast::channel(512);
        let workspace = Workspace::new("default", default_workspace_root());
        let agent = Agent::new("default", "Default Agent", workspace.id.clone());
        let mut default_settings = RuntimeSettings::with_defaults("default", model_name.into());
        if let Some(role) = env_role(ModelRole::Compressor) {
            default_settings.model_roles.push(role);
        }
        if let Some(role) = env_role(ModelRole::Embeddings) {
            default_settings.model_roles.push(role);
        }
        let default_retention = RetentionSettings::with_defaults("default");
        db.seed_default_agent(&agent, &workspace)
            .await
            .map_err(RuntimeError::from)?;
        db.seed_runtime_settings(&default_settings)
            .await
            .map_err(RuntimeError::from)?;
        db.seed_retention_settings(&default_retention)
            .await
            .map_err(RuntimeError::from)?;

        let runtime = Self {
            db,
            tools: ToolRegistry::with_defaults(),
            model_engine: Arc::new(model_engine),
            provider_name: provider_name.into(),
            provider_throttle: Arc::new(ProviderThrottle::new(base_backoff)),
            provider_request_gate: Arc::new(tokio::sync::Mutex::new(())),
            updates,
        };
        runtime.apply_startup_setting_overrides("default").await?;
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

    pub async fn get_retention_settings(
        &self,
        agent_id: &str,
    ) -> Result<RetentionSettings, RuntimeError> {
        self.db
            .load_retention_settings(agent_id)
            .await
            .map_err(RuntimeError::from)?
            .ok_or_else(|| RuntimeError::AgentNotFound(agent_id.to_string()))
    }

    pub async fn update_retention_settings(
        &self,
        mut settings: RetentionSettings,
    ) -> Result<RetentionSettings, RuntimeError> {
        let existing = self
            .db
            .load_retention_settings(&settings.agent_id)
            .await
            .map_err(RuntimeError::from)?;
        let created_at = existing
            .as_ref()
            .map(|item| item.created_at)
            .unwrap_or(settings.created_at);
        settings.created_at = created_at;
        settings.updated_at = Utc::now();
        self.db
            .update_retention_settings(&settings)
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
                        "parameters": normalize_tool_parameters_schema(&tool.parameters_schema),
                        "strict": true
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

    pub async fn prune_trace_blobs(
        &self,
        agent_id: &str,
    ) -> Result<TracePruneReport, RuntimeError> {
        let settings = self.get_retention_settings(agent_id).await?;
        if settings.trace_blob_retention_days == 0 {
            return Ok(TracePruneReport {
                retention_days: 0,
                pruned_blob_count: 0,
                reclaimed_bytes: 0,
            });
        }
        let now = Utc::now();
        let cutoff = now - chrono::Duration::days(settings.trace_blob_retention_days as i64);
        let report = self
            .db
            .prune_trace_blobs_older_than(cutoff, now)
            .await
            .map_err(RuntimeError::from)?;
        Ok(TracePruneReport {
            retention_days: settings.trace_blob_retention_days,
            pruned_blob_count: report.pruned_blob_count,
            reclaimed_bytes: report.reclaimed_bytes,
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
                metadata: None,
                received_at: Utc::now(),
            },
            Some(source_turn_id.to_string()),
        )
        .await
    }

    pub async fn handle_inbound(&self, event: InboundEvent) -> Result<TurnOutcome, RuntimeError> {
        self.handle_inbound_internal(event, None).await
    }
}
