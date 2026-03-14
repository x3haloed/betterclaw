use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InboundEvent {
    pub agent_id: String,
    pub channel: String,
    pub external_thread_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub received_at: DateTime<Utc>,
}

impl InboundEvent {
    pub fn web(
        agent_id: impl Into<String>,
        thread_id: impl Into<String>,
        content: impl Into<String>,
    ) -> Self {
        Self {
            agent_id: agent_id.into(),
            channel: "web".to_string(),
            external_thread_id: thread_id.into(),
            content: content.into(),
            metadata: None,
            received_at: Utc::now(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OutboundMessage {
    pub id: String,
    pub turn_id: String,
    pub thread_id: String,
    pub channel: String,
    pub external_thread_id: String,
    pub content: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub metadata: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ChannelCursor {
    pub channel: String,
    pub cursor_key: String,
    pub cursor_value: String,
    pub updated_at: DateTime<Utc>,
}
