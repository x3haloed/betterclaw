//! History record types used across the app.
//!
//! BetterClaw stores history via the `crate::db::Database` trait (libsql backend).
//! This module contains the shared record structs that are returned by DB queries
//! and surfaced in the UI/API layers.

use chrono::{DateTime, Utc};
use rust_decimal::Decimal;
use uuid::Uuid;

/// Record for an LLM call to be persisted.
#[derive(Debug, Clone)]
pub struct LlmCallRecord<'a> {
    pub job_id: Option<Uuid>,
    pub conversation_id: Option<Uuid>,
    pub provider: &'a str,
    pub model: &'a str,
    pub input_tokens: u32,
    pub output_tokens: u32,
    pub cost: Decimal,
    pub purpose: Option<&'a str>,
}

// ==================== Sandbox Jobs ====================

/// Record for a sandbox container job.
#[derive(Debug, Clone)]
pub struct SandboxJobRecord {
    pub id: Uuid,
    pub task: String,
    pub status: String,
    pub user_id: String,
    pub project_dir: String,
    pub success: Option<bool>,
    pub failure_reason: Option<String>,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    /// Serialized JSON of `Vec<CredentialGrant>` for restart support.
    pub credential_grants_json: String,
}

/// Summary of sandbox job counts grouped by status.
#[derive(Debug, Clone, Default)]
pub struct SandboxJobSummary {
    pub total: usize,
    pub creating: usize,
    pub running: usize,
    pub completed: usize,
    pub failed: usize,
    pub interrupted: usize,
}

/// Lightweight record for agent (non-sandbox) jobs, used by the web Jobs tab.
#[derive(Debug, Clone)]
pub struct AgentJobRecord {
    pub id: Uuid,
    pub title: String,
    pub status: String,
    pub user_id: String,
    pub created_at: DateTime<Utc>,
    pub started_at: Option<DateTime<Utc>>,
    pub completed_at: Option<DateTime<Utc>>,
    pub failure_reason: Option<String>,
}

/// Summary counts for agent (non-sandbox) jobs.
#[derive(Debug, Clone, Default)]
pub struct AgentJobSummary {
    pub total: usize,
    pub pending: usize,
    pub in_progress: usize,
    pub completed: usize,
    pub failed: usize,
    pub stuck: usize,
}

impl AgentJobSummary {
    /// Accumulate a status/count pair into the summary buckets.
    pub fn add_count(&mut self, status: &str, count: usize) {
        self.total += count;
        match status {
            "pending" => self.pending += count,
            "in_progress" => self.in_progress += count,
            "completed" | "submitted" | "accepted" => self.completed += count,
            "failed" | "cancelled" => self.failed += count,
            "stuck" => self.stuck += count,
            _ => {}
        }
    }
}

// ==================== Job Events ====================

/// A persisted job streaming event (from worker or Claude Code bridge).
#[derive(Debug, Clone)]
pub struct JobEventRecord {
    pub id: i64,
    pub job_id: Uuid,
    pub event_type: String,
    pub data: serde_json::Value,
    pub created_at: DateTime<Utc>,
}

// ==================== Conversation Persistence ====================

/// Summary of a conversation for the thread list.
#[derive(Debug, Clone)]
pub struct ConversationSummary {
    pub id: Uuid,
    /// First user message, truncated to 100 chars.
    pub title: Option<String>,
    pub message_count: i64,
    pub started_at: DateTime<Utc>,
    pub last_activity: DateTime<Utc>,
    /// Thread type extracted from metadata (e.g. "assistant", "thread").
    pub thread_type: Option<String>,
}

/// A single message in a conversation.
#[derive(Debug, Clone)]
pub struct ConversationMessage {
    pub id: Uuid,
    pub role: String,
    pub content: String,
    pub created_at: DateTime<Utc>,
}

// ==================== Settings ====================

/// A single setting row from the database.
#[derive(Debug, Clone)]
pub struct SettingRow {
    pub key: String,
    pub value: serde_json::Value,
    pub updated_at: DateTime<Utc>,
}
