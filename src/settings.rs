use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ModelRole {
    Agent,
    Compressor,
    Embeddings,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelRoleConfig {
    pub role: ModelRole,
    pub provider: String,
    pub mode: Option<String>,
    pub model: String,
    pub base_url: Option<String>,
    pub api_key_env_var: Option<String>,
    #[serde(default)]
    pub extra_headers: Vec<(String, String)>,
    #[serde(default = "default_role_enabled")]
    pub enabled: bool,
}

fn default_role_enabled() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettings {
    pub agent_id: String,
    pub model: String,
    pub system_prompt: String,
    pub max_tokens: u32,
    pub stream: bool,
    pub allow_tools: bool,
    pub max_history_turns: u32,
    #[serde(default = "default_true")]
    pub inject_wake_pack: bool,
    #[serde(default = "default_true")]
    pub inject_ledger_recall: bool,
    #[serde(default = "default_true")]
    pub enable_auto_distill: bool,
    #[serde(default = "default_true")]
    pub enable_observations: bool,
    #[serde(default = "default_true")]
    pub inject_observations: bool,
    #[serde(default)]
    pub model_roles: Vec<ModelRoleConfig>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

fn default_true() -> bool {
    true
}

pub fn default_system_prompt() -> String {
    r#"You are BetterClaw Agent, a secure autonomous assistant.

## Guidelines
- Be concise and direct
- Use markdown formatting where helpful
- For code, use appropriate code blocks with language tags
- ALWAYS call tools via tool_calls when they materially help — never just describe what you would do
- If you say you will fetch, check, inspect, or search, include the actual tool call in the same response
- For multi-step tasks, call independent tools in parallel when possible
- If a tool fails, explain the error briefly and try an alternative approach"#
        .to_string()
}

impl RuntimeSettings {
    pub fn with_defaults(agent_id: impl Into<String>, model: impl Into<String>) -> Self {
        let now = Utc::now();
        let model = model.into();
        Self {
            agent_id: agent_id.into(),
            model: model.clone(),
            system_prompt: default_system_prompt(),
            max_tokens: 1024,
            stream: true,
            allow_tools: true,
            max_history_turns: 12,
            inject_wake_pack: true,
            inject_ledger_recall: true,
            enable_auto_distill: true,
            enable_observations: true,
            inject_observations: true,
            model_roles: vec![ModelRoleConfig {
                role: ModelRole::Agent,
                provider: "local".to_string(),
                mode: Some("chat".to_string()),
                model,
                base_url: None,
                api_key_env_var: None,
                extra_headers: Vec::new(),
                enabled: true,
            }],
            created_at: now,
            updated_at: now,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RetentionSettings {
    pub agent_id: String,
    pub trace_blob_retention_days: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RetentionSettings {
    pub fn with_defaults(agent_id: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            agent_id: agent_id.into(),
            trace_blob_retention_days: 0,
            created_at: now,
            updated_at: now,
        }
    }
}
