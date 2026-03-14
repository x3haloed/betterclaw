use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: Uuid,
    pub thread_id: Uuid,
    pub timestamp: DateTime<Utc>,
    pub kind: EventKind,
    pub payload: Value,
}

impl Event {
    pub fn new(thread_id: Uuid, kind: EventKind, payload: Value) -> Self {
        Self {
            id: Uuid::new_v4(),
            thread_id,
            timestamp: Utc::now(),
            kind,
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    InboundMessage,
    ContextAssembled,
    LlmRequest,
    LlmResponse,
    ToolCall,
    ToolResult,
    CursorAdvanced,
    OutboundMessage,
    Error,
}
