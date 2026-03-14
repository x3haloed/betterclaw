use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Thread {
    pub id: Uuid,
    pub agent_id: String,
    pub channel: String,
    pub external_thread_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

impl Thread {
    pub fn new(
        agent_id: impl Into<String>,
        channel: impl Into<String>,
        external_thread_id: Option<String>,
    ) -> Self {
        let now = Utc::now();
        Self {
            id: Uuid::new_v4(),
            agent_id: agent_id.into(),
            channel: channel.into(),
            external_thread_id,
            created_at: now,
            updated_at: now,
        }
    }
}
