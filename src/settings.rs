use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeSettings {
    pub agent_id: String,
    pub model: String,
    pub system_prompt: String,
    pub temperature: f32,
    pub max_tokens: u32,
    pub stream: bool,
    pub allow_tools: bool,
    pub max_history_turns: u32,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl RuntimeSettings {
    pub fn with_defaults(agent_id: impl Into<String>, model: impl Into<String>) -> Self {
        let now = Utc::now();
        Self {
            agent_id: agent_id.into(),
            model: model.into(),
            system_prompt: "You are BetterClaw, a host-native AI operator. Be helpful, direct, and use tools when they materially improve the answer.".to_string(),
            temperature: 0.2,
            max_tokens: 1024,
            stream: true,
            allow_tools: true,
            max_history_turns: 12,
            created_at: now,
            updated_at: now,
        }
    }
}
