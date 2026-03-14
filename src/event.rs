use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Event {
    pub id: String,
    pub turn_id: String,
    pub thread_id: String,
    pub sequence: i64,
    pub kind: EventKind,
    pub payload: Value,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EventKind {
    InboundMessage,
    ThreadResolved,
    ReplayRequested,
    TurnRecovered,
    ContextAssembled,
    ModelRequest,
    ModelResponse,
    ToolCall,
    ToolResult,
    OutboundMessage,
    Error,
}
