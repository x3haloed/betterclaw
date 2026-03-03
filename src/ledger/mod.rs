//! Append-only ledger primitives.
//!
//! The ledger is the canonical, cited source-of-truth for recall and distillation.
//! Workspace files are just files; they are not "memory".

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

/// A single immutable event in the ledger.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEvent {
    pub id: Uuid,
    pub user_id: String,
    pub episode_id: Option<Uuid>,
    /// Event kind (e.g. `user_turn`, `agent_turn`, `tool_call`, `tool_result`, `decision`).
    pub kind: String,
    /// Source system (e.g. `web`, `discord`, `gateway`, `tool:<name>`).
    pub source: String,
    /// Optional verbatim text content for citation.
    pub content: Option<String>,
    /// Structured payload for tooling/distillation.
    pub payload: serde_json::Value,
    /// Optional integrity hash of the stored event fields.
    pub sha256: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Input for creating a new ledger event.
#[derive(Debug, Clone)]
pub struct NewLedgerEvent<'a> {
    pub user_id: &'a str,
    pub episode_id: Option<Uuid>,
    pub kind: &'a str,
    pub source: &'a str,
    pub content: Option<&'a str>,
    pub payload: &'a serde_json::Value,
}

