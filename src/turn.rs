use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::channel::InboundAttachment;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Turn {
    pub id: String,
    pub thread_id: String,
    pub status: TurnStatus,
    pub user_message: String,
    /// JSON-serialized attachments from the inbound event.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attachments_json: Option<String>,
    pub assistant_message: Option<String>,
    pub error: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Turn {
    /// Deserialize stored attachments back into typed structs.
    pub fn attachments(&self) -> Vec<InboundAttachment> {
        self.attachments_json
            .as_deref()
            .and_then(|json| serde_json::from_str(json).ok())
            .unwrap_or_default()
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TurnStatus {
    Pending,
    Running,
    AwaitingUser,
    Succeeded,
    Failed,
}
