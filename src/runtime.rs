use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, Instant};

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
use crate::memory::{
    LedgerEntry, LedgerEntryKind, MemoryArtifactKind, NewMemoryArtifact, RecallHit, chunk_text,
    cosine_similarity,
};
use crate::model::{
    ModelEngine, ModelEngineError, ModelExchangeRequest, ModelExchangeResult, ModelMessage,
    ModelToolCallMessage, ModelToolFunctionMessage, ModelTrace, OpenAiChatCompletionsEngine,
    OpenAiCompatibleConfig, OpenAiResponsesEngine, ReducedToolCall, StubModelEngine, TraceDetail,
    TraceOutcome, strip_reasoning_tags,
};
use crate::settings::{ModelRole, ModelRoleConfig, RetentionSettings, RuntimeSettings};
use crate::thread::Thread;
use crate::tool::{ToolContext, ToolInvocation, ToolRegistry, ToolResult};
use crate::turn::{Turn, TurnStatus};
use crate::workspace::Workspace;

#[derive(Clone)]
pub struct Runtime {
    db: Arc<Db>,
    tools: ToolRegistry,
    model_engine: Arc<ModelEngine>,
    provider_name: String,
    provider_throttle: Arc<ProviderThrottle>,
    provider_request_gate: Arc<tokio::sync::Mutex<()>>,
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

#[derive(Debug, Clone, serde::Serialize)]
pub struct TracePruneReport {
    pub retention_days: u32,
    pub pruned_blob_count: usize,
    pub reclaimed_bytes: i64,
}

struct ResolvedModelEngine {
    engine: ModelEngine,
    model_name: String,
    provider_name: String,
}

struct ProviderPreset;

#[derive(Clone)]
struct EmbeddingClient {
    client: reqwest::Client,
    base_url: String,
    provider_name: String,
    model: String,
}

impl ProviderPreset {
    fn from_env() -> Result<ResolvedModelEngine, anyhow::Error> {
        let provider = std::env::var("BETTERCLAW_PROVIDER")
            .unwrap_or_else(|_| "local".to_string())
            .to_lowercase();
        match provider.as_str() {
            "local" | "lmstudio" | "openai_compatible" => Self::local_from_env(),
            "openrouter" => Self::openrouter_from_env(),
            "codex" => Self::codex_from_env(),
            other => anyhow::bail!("unsupported BETTERCLAW_PROVIDER '{other}'"),
        }
    }

    fn local_from_env() -> Result<ResolvedModelEngine, anyhow::Error> {
        let model_name =
            std::env::var("BETTERCLAW_MODEL").unwrap_or_else(|_| "qwen/qwen3.5-9b".to_string());
        let base_url = std::env::var("BETTERCLAW_MODEL_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:1234/v1".to_string());
        let engine = OpenAiChatCompletionsEngine::new(OpenAiCompatibleConfig {
            base_url,
            provider_name: "local-openai-compatible".to_string(),
            ..OpenAiCompatibleConfig::default()
        })?;
        Ok(ResolvedModelEngine {
            engine: ModelEngine::openai_chat_completions(engine),
            model_name,
            provider_name: "local-openai-compatible".to_string(),
        })
    }

    fn openrouter_from_env() -> Result<ResolvedModelEngine, anyhow::Error> {
        let mode = std::env::var("BETTERCLAW_PROVIDER_MODE")
            .or_else(|_| std::env::var("OPENROUTER_MODE"))
            .unwrap_or_else(|_| "chat".to_string())
            .to_lowercase();
        let model_name = std::env::var("OPENROUTER_MODEL")
            .or_else(|_| std::env::var("BETTERCLAW_MODEL"))
            .unwrap_or_else(|_| "openai/gpt-4o-mini".to_string());
        let base_url = std::env::var("OPENROUTER_BASE_URL")
            .or_else(|_| std::env::var("BETTERCLAW_MODEL_BASE_URL"))
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
        let api_key = std::env::var("OPENROUTER_API_KEY").ok();
        let mut extra_headers = Vec::new();
        if let Ok(referer) = std::env::var("OPENROUTER_HTTP_REFERER") {
            extra_headers.push(("HTTP-Referer".to_string(), referer));
        }
        if let Ok(title) = std::env::var("OPENROUTER_X_TITLE") {
            extra_headers.push(("X-Title".to_string(), title));
        }
        let config = OpenAiCompatibleConfig {
            base_url,
            provider_name: "openrouter".to_string(),
            bearer_token: api_key,
            extra_headers,
            ..OpenAiCompatibleConfig::default()
        };
        let engine = match mode.as_str() {
            "responses" => ModelEngine::openai_responses(OpenAiResponsesEngine::new(config)?),
            "chat" | "chat_completions" => {
                ModelEngine::openai_chat_completions(OpenAiChatCompletionsEngine::new(config)?)
            }
            other => anyhow::bail!("unsupported OpenRouter mode '{other}'"),
        };
        Ok(ResolvedModelEngine {
            engine,
            model_name,
            provider_name: "openrouter".to_string(),
        })
    }

    fn codex_from_env() -> Result<ResolvedModelEngine, anyhow::Error> {
        let auth_path = std::env::var("OPENAI_CODEX_AUTH_PATH")
            .unwrap_or_else(|_| default_openai_codex_auth_path());
        let (token, account_id) = load_openai_codex_credentials(&auth_path)?;
        let model_name = std::env::var("OPENAI_CODEX_MODEL")
            .or_else(|_| std::env::var("BETTERCLAW_MODEL"))
            .unwrap_or_else(|_| "gpt-5-codex".to_string());
        let base_url = std::env::var("OPENAI_CODEX_BASE_URL")
            .or_else(|_| std::env::var("BETTERCLAW_MODEL_BASE_URL"))
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".to_string());
        let mut extra_headers = Vec::new();
        if let Some(account_id) = account_id {
            extra_headers.push(("ChatGPT-Account-Id".to_string(), account_id));
        }
        let engine = OpenAiResponsesEngine::new(OpenAiCompatibleConfig {
            base_url,
            provider_name: "codex".to_string(),
            bearer_token: Some(token),
            extra_headers,
            ..OpenAiCompatibleConfig::default()
        })?;
        Ok(ResolvedModelEngine {
            engine: ModelEngine::openai_responses(engine),
            model_name,
            provider_name: "codex".to_string(),
        })
    }
}

impl EmbeddingClient {
    fn new(role: &ModelRoleConfig) -> Result<Self, anyhow::Error> {
        let mut config = OpenAiCompatibleConfig {
            base_url: role
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:1234/v1".to_string()),
            provider_name: role.provider.clone(),
            extra_headers: role.extra_headers.clone(),
            ..OpenAiCompatibleConfig::default()
        };
        if let Some(env_var) = &role.api_key_env_var {
            config.bearer_token = std::env::var(env_var).ok();
        } else {
            config.bearer_token = match role.provider.as_str() {
                "openrouter" => std::env::var("OPENROUTER_API_KEY").ok(),
                "codex" => std::env::var("OPENAI_API_KEY").ok(),
                _ => std::env::var("BETTERCLAW_EMBEDDINGS_API_KEY").ok(),
            };
        }
        Ok(Self {
            client: config.build_client(false)?,
            base_url: config.base_url,
            provider_name: config.provider_name,
            model: role.model.clone(),
        })
    }

    async fn embed(&self, input: &str) -> Result<Vec<f32>, RuntimeError> {
        let response = self
            .client
            .post(format!(
                "{}/embeddings",
                self.base_url.trim_end_matches('/')
            ))
            .json(&json!({
                "model": self.model,
                "input": input,
            }))
            .send()
            .await
            .map_err(|error| {
                RuntimeError::ModelParse(format!(
                    "{} embeddings transport failure: {error}",
                    self.provider_name
                ))
            })?;
        let status = response.status();
        let body: Value = response.json().await.map_err(|error| {
            RuntimeError::ModelParse(format!(
                "{} embeddings decode failure: {error}",
                self.provider_name
            ))
        })?;
        if !status.is_success() {
            return Err(RuntimeError::ModelParse(format!(
                "{} embeddings returned HTTP {}",
                self.provider_name,
                status.as_u16()
            )));
        }
        let embedding = body
            .get("data")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("embedding"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                RuntimeError::ModelParse(
                    "embeddings response missing data[0].embedding".to_string(),
                )
            })?;
        let mut values = Vec::with_capacity(embedding.len());
        for value in embedding {
            values.push(value.as_f64().unwrap_or_default() as f32);
        }
        Ok(values)
    }
}

#[derive(Debug)]
struct ProviderThrottle {
    base_backoff: Duration,
    state: tokio::sync::Mutex<ProviderThrottleState>,
}

#[derive(Debug)]
struct ProviderThrottleState {
    blocked_until: Option<tokio::time::Instant>,
    next_backoff: Duration,
}

impl ProviderThrottle {
    fn new(base_backoff: Duration) -> Self {
        Self {
            base_backoff,
            state: tokio::sync::Mutex::new(ProviderThrottleState {
                blocked_until: None,
                next_backoff: base_backoff,
            }),
        }
    }

    async fn current_wait(&self) -> Option<Duration> {
        let mut state = self.state.lock().await;
        let Some(blocked_until) = state.blocked_until else {
            return None;
        };
        let now = tokio::time::Instant::now();
        if blocked_until <= now {
            state.blocked_until = None;
            return None;
        }
        Some(blocked_until.duration_since(now))
    }

    async fn arm(&self, retry_after: Option<Duration>) -> Duration {
        let mut state = self.state.lock().await;
        let wait = match retry_after {
            Some(wait) => {
                state.next_backoff = self.base_backoff;
                wait
            }
            None => {
                let wait = state.next_backoff;
                state.next_backoff = state.next_backoff.checked_mul(2).unwrap_or(Duration::MAX);
                wait
            }
        };
        let now = tokio::time::Instant::now();
        let candidate = now + wait;
        state.blocked_until = Some(match state.blocked_until {
            Some(existing) if existing > candidate => existing,
            _ => candidate,
        });
        state
            .blocked_until
            .map(|deadline| deadline.duration_since(now))
            .unwrap_or(wait)
    }

    async fn note_success(&self) {
        let mut state = self.state.lock().await;
        state.next_backoff = self.base_backoff;
        if let Some(blocked_until) = state.blocked_until
            && blocked_until <= tokio::time::Instant::now()
        {
            state.blocked_until = None;
        }
    }
}

fn default_openai_codex_auth_path() -> String {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".codex")
        .join("auth.json")
        .display()
        .to_string()
}

fn load_openai_codex_credentials(
    auth_file: &str,
) -> Result<(String, Option<String>), anyhow::Error> {
    let contents = std::fs::read_to_string(auth_file)?;
    let parsed: Value = serde_json::from_str(&contents)?;
    let tokens = parsed
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("Codex auth file is missing the 'tokens' object"))?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Codex auth file is missing tokens.access_token"))?;
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Ok((access_token.to_string(), account_id))
}

fn system_prompt_override_from_env() -> Option<String> {
    std::env::var("BETTERCLAW_SYSTEM_PROMPT")
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
}

fn default_memory_namespace() -> String {
    "default".to_string()
}

fn truncate_for_wake_pack(text: &str) -> String {
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

fn build_fts_query(query: &str) -> String {
    query
        .split(|ch: char| !(ch.is_ascii_alphanumeric() || ch == '_' || ch == '-'))
        .filter(|token| !token.trim().is_empty())
        .take(8)
        .map(|token| format!("\"{}\"", token.replace('"', "\"\"")))
        .collect::<Vec<_>>()
        .join(" OR ")
}

fn env_role(role: ModelRole) -> Option<ModelRoleConfig> {
    match role {
        ModelRole::Agent => None,
        ModelRole::Compressor => {
            let provider = std::env::var("BETTERCLAW_COMPRESSOR_PROVIDER").ok()?;
            Some(ModelRoleConfig {
                role,
                provider,
                mode: std::env::var("BETTERCLAW_COMPRESSOR_MODE").ok(),
                model: std::env::var("BETTERCLAW_COMPRESSOR_MODEL")
                    .unwrap_or_else(|_| "gpt-4o-mini".to_string()),
                base_url: std::env::var("BETTERCLAW_COMPRESSOR_BASE_URL").ok(),
                api_key_env_var: std::env::var("BETTERCLAW_COMPRESSOR_API_KEY_ENV").ok(),
                extra_headers: Vec::new(),
                enabled: true,
            })
        }
        ModelRole::Embeddings => {
            let model = std::env::var("BETTERCLAW_EMBEDDINGS_MODEL").ok()?;
            Some(ModelRoleConfig {
                role,
                provider: std::env::var("BETTERCLAW_EMBEDDINGS_PROVIDER")
                    .unwrap_or_else(|_| "openai_compatible".to_string()),
                mode: None,
                model,
                base_url: std::env::var("BETTERCLAW_EMBEDDINGS_BASE_URL").ok(),
                api_key_env_var: std::env::var("BETTERCLAW_EMBEDDINGS_API_KEY_ENV").ok(),
                extra_headers: Vec::new(),
                enabled: true,
            })
        }
    }
}

impl Runtime {
    fn parse_tool_control(output: &Value) -> Option<ToolControl> {
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
            _ => None,
        }
    }

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
        let workspace = Workspace::new(
            "default",
            std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        );
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

    async fn apply_startup_setting_overrides(&self, agent_id: &str) -> Result<(), RuntimeError> {
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
        let mut outbound_messages = Vec::new();
        let (final_response, last_trace_id, final_status) = loop {
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
            if let Some(question) = continuation_messages.ask_user_question {
                break (question, trace_id, TurnStatus::AwaitingUser);
            }
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
        let completed_turn = self
            .get_turn(&turn.id)
            .await?
            .ok_or_else(|| RuntimeError::TurnNotFound(turn.id.clone()))?;
        self.sync_memory_for_turn(&thread, &completed_turn, &settings)
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
        outbound_messages.push(final_response.clone());

        Ok(TurnOutcome {
            thread,
            turn_id: turn.id,
            response: final_response,
            trace_id: last_trace_id,
            status: final_status,
            outbound_messages,
        })
    }

    async fn build_conversation_history(
        &self,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<Vec<ModelMessage>, RuntimeError> {
        let mut messages = self
            .build_system_messages(settings, Some(&turn.user_message))
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

    async fn build_system_messages(
        &self,
        settings: &RuntimeSettings,
        query_hint: Option<&str>,
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
        let namespace = default_memory_namespace();
        if settings.inject_wake_pack
            && let Some(wake_pack) = self
                .db
                .latest_memory_artifact(&namespace, MemoryArtifactKind::WakePackV0)
                .await
                .map_err(RuntimeError::from)?
        {
            messages.push(ModelMessage {
                role: "system".to_string(),
                content: Some(format!("<wake_pack>\n{}\n</wake_pack>", wake_pack.content)),
                tool_calls: None,
                tool_call_id: None,
            });
        }
        if settings.inject_ledger_recall {
            if let Some(recall_block) = self
                .build_ledger_recall_block(
                    &namespace,
                    query_hint.unwrap_or(&settings.system_prompt),
                )
                .await?
            {
                messages.push(ModelMessage {
                    role: "system".to_string(),
                    content: Some(recall_block),
                    tool_calls: None,
                    tool_call_id: None,
                });
            }
        }
        Ok(messages)
    }

    async fn build_ledger_recall_block(
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

    async fn search_recall(
        &self,
        namespace_id: &str,
        query: &str,
        limit: usize,
    ) -> Result<Vec<RecallHit>, RuntimeError> {
        let mut scores = std::collections::HashMap::<String, RecallHit>::new();
        let lexical_query = build_fts_query(query);
        if !lexical_query.trim().is_empty() {
            for hit in self
                .db
                .search_recall_chunks_keyword(namespace_id, &lexical_query, limit as i64 * 2)
                .await
                .map_err(RuntimeError::from)?
            {
                scores
                    .entry(hit.entry_id.clone())
                    .and_modify(|current| current.score += hit.score.max(0.1))
                    .or_insert(hit);
            }
        }
        if let Some(client) = self.embedding_client_for_namespace(namespace_id).await? {
            let query_embedding = client.embed(query).await?;
            for chunk in self
                .db
                .list_recall_chunks_with_embeddings(namespace_id, 256)
                .await
                .map_err(RuntimeError::from)?
            {
                let Some(embedding_json) = &chunk.embedding_json else {
                    continue;
                };
                let Ok(values) = serde_json::from_str::<Vec<f32>>(embedding_json) else {
                    continue;
                };
                let Some(score) = cosine_similarity(&query_embedding, &values) else {
                    continue;
                };
                let hit = RecallHit {
                    entry_id: chunk.entry_id.clone(),
                    source_id: chunk.source_id.clone(),
                    source_type: chunk.source_type.clone(),
                    content: chunk.content.clone(),
                    score,
                    citation: Some(chunk.entry_id.clone()),
                };
                scores
                    .entry(hit.entry_id.clone())
                    .and_modify(|current| {
                        if score > current.score {
                            current.score = score;
                            current.content = hit.content.clone();
                            current.citation = hit.citation.clone();
                        }
                    })
                    .or_insert(hit);
            }
        }
        let mut hits = scores.into_values().collect::<Vec<_>>();
        hits.sort_by(|left, right| {
            right
                .score
                .partial_cmp(&left.score)
                .unwrap_or(std::cmp::Ordering::Equal)
        });
        hits.truncate(limit);
        Ok(hits)
    }

    async fn embedding_client_for_namespace(
        &self,
        _namespace_id: &str,
    ) -> Result<Option<EmbeddingClient>, RuntimeError> {
        let settings = self.get_runtime_settings("default").await?;
        let role = settings
            .model_roles
            .iter()
            .find(|role| role.role == ModelRole::Embeddings && role.enabled)
            .cloned()
            .or_else(|| env_role(ModelRole::Embeddings));
        role.map(|role| EmbeddingClient::new(&role).map_err(RuntimeError::from))
            .transpose()
    }

    async fn sync_memory_for_turn(
        &self,
        thread: &Thread,
        turn: &Turn,
        settings: &RuntimeSettings,
    ) -> Result<(), RuntimeError> {
        let namespace = default_memory_namespace();
        let entries = self.normalized_entries_for_turn(thread, turn).await?;
        let embedder = self.embedding_client_for_namespace(&namespace).await?;
        for entry in entries {
            let content = entry
                .content
                .clone()
                .unwrap_or_else(|| entry.payload.to_string());
            let chunks = chunk_text(&content, 1200);
            let mut stored_chunks = Vec::new();
            for chunk in chunks {
                let embedding_json = match &embedder {
                    Some(client) => Some(
                        serde_json::to_string(&client.embed(&chunk).await?)
                            .map_err(|error| RuntimeError::ModelParse(error.to_string()))?,
                    ),
                    None => None,
                };
                stored_chunks.push((chunk, embedding_json));
            }
            self.db
                .replace_recall_chunks_for_source(
                    &namespace,
                    "ledger_entry",
                    &entry.entry_id,
                    &entry.entry_id,
                    &stored_chunks,
                )
                .await
                .map_err(RuntimeError::from)?;
        }
        if settings.enable_auto_distill {
            self.auto_distill_namespace(&namespace).await?;
        }
        Ok(())
    }

    async fn auto_distill_namespace(&self, namespace_id: &str) -> Result<(), RuntimeError> {
        let entries = self.normalized_entries_for_namespace(namespace_id).await?;
        if entries.is_empty() {
            return Ok(());
        }
        let recent = entries.into_iter().rev().take(8).collect::<Vec<_>>();
        let mut lines = Vec::new();
        let mut citations = Vec::new();
        for entry in recent.iter().rev() {
            citations.push(entry.entry_id.clone());
            match entry.kind {
                LedgerEntryKind::UserTurn => {
                    if let Some(content) = &entry.content {
                        lines.push(format!("- User asked: {}", truncate_for_wake_pack(content)));
                    }
                }
                LedgerEntryKind::AgentTurn => {
                    if let Some(content) = &entry.content {
                        lines.push(format!(
                            "- Agent answered: {}",
                            truncate_for_wake_pack(content)
                        ));
                    }
                }
                LedgerEntryKind::ToolResult => {
                    lines.push(format!(
                        "- Tool result observed: {}",
                        truncate_for_wake_pack(&entry.payload.to_string())
                    ));
                }
                _ => {}
            }
        }
        if lines.is_empty() {
            return Ok(());
        }
        let content = format!("# Wake Pack (v0)\n\n{}\n", lines.join("\n"));
        let prior = self
            .db
            .latest_memory_artifact(namespace_id, MemoryArtifactKind::WakePackV0)
            .await
            .map_err(RuntimeError::from)?;
        let wake_pack = self
            .db
            .upsert_memory_artifact(&NewMemoryArtifact {
                namespace_id: namespace_id.to_string(),
                kind: MemoryArtifactKind::WakePackV0,
                source: "compressor".to_string(),
                content: content.clone(),
                payload: json!({
                    "line_count": lines.len(),
                    "strategy": "deterministic_recent_runtime_summary",
                }),
                citations: citations.clone(),
                supersedes_id: prior.as_ref().map(|artifact| artifact.id.clone()),
            })
            .await
            .map_err(RuntimeError::from)?;
        self.db
            .upsert_memory_artifact(&NewMemoryArtifact {
                namespace_id: namespace_id.to_string(),
                kind: MemoryArtifactKind::DistillMicro,
                source: "compressor".to_string(),
                content: String::new(),
                payload: json!({
                    "wake_pack_id": wake_pack.id,
                    "citations": citations,
                }),
                citations: prior.map(|artifact| vec![artifact.id]).unwrap_or_default(),
                supersedes_id: None,
            })
            .await
            .map_err(RuntimeError::from)?;
        Ok(())
    }

    async fn normalized_entries_for_namespace(
        &self,
        namespace_id: &str,
    ) -> Result<Vec<LedgerEntry>, RuntimeError> {
        let mut entries = Vec::new();
        for thread in self.db.list_threads().await.map_err(RuntimeError::from)? {
            for turn in self
                .db
                .list_thread_turns(&thread.id)
                .await
                .map_err(RuntimeError::from)?
            {
                entries.extend(self.normalized_entries_for_turn(&thread, &turn).await?);
            }
        }
        entries.sort_by_key(|entry| entry.created_at);
        for entry in &mut entries {
            entry.namespace_id = namespace_id.to_string();
        }
        Ok(entries)
    }

    async fn normalized_entries_for_turn(
        &self,
        thread: &Thread,
        turn: &Turn,
    ) -> Result<Vec<LedgerEntry>, RuntimeError> {
        let mut entries = vec![LedgerEntry {
            entry_id: format!("turn:{}:user", turn.id),
            namespace_id: default_memory_namespace(),
            turn_id: turn.id.clone(),
            thread_id: thread.id.clone(),
            kind: LedgerEntryKind::UserTurn,
            source: thread.channel.clone(),
            content: Some(turn.user_message.clone()),
            payload: json!({ "thread_id": thread.id, "turn_id": turn.id }),
            citation: format!("turn:{}", turn.id),
            created_at: turn.created_at,
        }];
        if let Some(assistant_message) = &turn.assistant_message {
            entries.push(LedgerEntry {
                entry_id: format!("turn:{}:assistant", turn.id),
                namespace_id: default_memory_namespace(),
                turn_id: turn.id.clone(),
                thread_id: thread.id.clone(),
                kind: LedgerEntryKind::AgentTurn,
                source: thread.channel.clone(),
                content: Some(assistant_message.clone()),
                payload: json!({ "thread_id": thread.id, "turn_id": turn.id }),
                citation: format!("turn:{}", turn.id),
                created_at: turn.updated_at,
            });
        }
        let events = self
            .db
            .list_thread_events(&thread.id)
            .await
            .map_err(RuntimeError::from)?
            .into_iter()
            .filter(|event| event.turn_id == turn.id)
            .collect::<Vec<_>>();
        for event in events {
            let kind = match event.kind {
                EventKind::ToolCall => LedgerEntryKind::ToolCall,
                EventKind::ToolResult => LedgerEntryKind::ToolResult,
                EventKind::Error => LedgerEntryKind::Error,
                _ => continue,
            };
            entries.push(LedgerEntry {
                entry_id: format!("event:{}", event.id),
                namespace_id: default_memory_namespace(),
                turn_id: turn.id.clone(),
                thread_id: thread.id.clone(),
                kind,
                source: "runtime_event".to_string(),
                content: Some(event.payload.to_string()),
                payload: event.payload.clone(),
                citation: format!("event:{}", event.id),
                created_at: event.created_at,
            });
        }
        Ok(entries)
    }

    async fn run_and_record_exchange(
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

    async fn wait_for_provider_window(
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
    ) -> Result<ToolExecutionOutcome, RuntimeError> {
        let mut assistant_tool_calls = Vec::new();
        let mut continuation_messages = Vec::new();
        let mut outbound_messages = Vec::new();
        let mut ask_user_question = None;
        let mut batch = Vec::new();
        let tool_context = ToolContext::new(
            workspace.clone(),
            thread.id.clone(),
            thread.external_thread_id.clone(),
            thread.channel.clone(),
            self.db(),
        );

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
                .execute(&tool_call.name, arguments.clone(), &tool_context)
        });
        let outputs = join_all(executions).await;

        for ((tool_call, invocation, _arguments), output) in batch.into_iter().zip(outputs) {
            let output = output?;
            if let Some(control) = Self::parse_tool_control(&output) {
                match control {
                    ToolControl::Message { content } => outbound_messages.push(content),
                    ToolControl::AskUser { question } => {
                        if ask_user_question.is_none() {
                            ask_user_question = Some(question);
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
        Ok(ToolExecutionOutcome {
            continuation_messages: messages,
            outbound_messages,
            ask_user_question,
        })
    }

    async fn record_outbound_and_publish(
        &self,
        turn: &Turn,
        thread: &Thread,
        content: &str,
        metadata: Option<Value>,
    ) -> Result<(), RuntimeError> {
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
    pub status: TurnStatus,
    pub outbound_messages: Vec<String>,
}

struct ToolExecutionOutcome {
    continuation_messages: Vec<ModelMessage>,
    outbound_messages: Vec<String>,
    ask_user_question: Option<String>,
}

#[derive(Debug, Clone)]
enum ToolControl {
    Message { content: String },
    AskUser { question: String },
}

#[cfg(test)]
mod tests {
    use std::sync::{Mutex, OnceLock};
    use std::time::{Duration, Instant};

    use tempfile::tempdir;

    use super::{ProviderPreset, Runtime};
    use crate::channel::InboundEvent;
    use crate::db::Db;
    use crate::event::EventKind;
    use crate::model::{ModelEngine, StubModelEngine, strip_reasoning_tags};
    use crate::turn::TurnStatus;

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

    #[tokio::test]
    async fn rate_limited_turn_retries_after_retry_after_window() {
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
                    "/rate-limit-once 40",
                ))
                .await
                .unwrap()
        });

        tokio::time::sleep(Duration::from_millis(20)).await;

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

        assert!(second_started.elapsed() >= Duration::from_millis(30));
        assert!(first.response.contains("/rate-limit-once 40"));
        assert!(second.response.contains("hello while blocked"));

        let second_timeline = runtime
            .list_thread_timeline(&second.thread.id)
            .await
            .unwrap();
        assert!(second_timeline.iter().any(|event| {
            event.kind == EventKind::RateLimited
                && event
                    .payload
                    .get("shared_gate")
                    .and_then(serde_json::Value::as_bool)
                    == Some(true)
        }));
    }

    #[tokio::test]
    async fn missing_retry_after_uses_exponential_backoff_base() {
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
        let _guard = env_mutex().lock().unwrap();
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
        let _guard = env_mutex().lock().unwrap();
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
        let _guard = env_mutex().lock().unwrap();
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
    async fn startup_system_prompt_override_updates_runtime_settings() {
        let _guard = env_mutex().lock().unwrap();
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
}
