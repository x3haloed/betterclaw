use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum MemoryArtifactKind {
    WakePackV0,
    InvariantSelfV0,
    InvariantUserV0,
    InvariantRelationshipV0,
    DriftFlagV0,
    DriftContradictionV0,
    DriftMergeV0,
    DistillMicro,
}

impl MemoryArtifactKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::WakePackV0 => "wake_pack.v0",
            Self::InvariantSelfV0 => "invariant.self.v0",
            Self::InvariantUserV0 => "invariant.user.v0",
            Self::InvariantRelationshipV0 => "invariant.relationship.v0",
            Self::DriftFlagV0 => "drift.flag.v0",
            Self::DriftContradictionV0 => "drift.contradiction.v0",
            Self::DriftMergeV0 => "drift.merge.v0",
            Self::DistillMicro => "distill.micro",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryArtifact {
    pub id: String,
    pub namespace_id: String,
    pub kind: MemoryArtifactKind,
    pub source: String,
    pub content: String,
    pub payload: Value,
    pub citations: Vec<String>,
    pub supersedes_id: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NewMemoryArtifact {
    pub namespace_id: String,
    pub kind: MemoryArtifactKind,
    pub source: String,
    pub content: String,
    pub payload: Value,
    pub citations: Vec<String>,
    pub supersedes_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum LedgerEntryKind {
    UserTurn,
    AgentTurn,
    ToolCall,
    ToolResult,
    Error,
    TraceSummary,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LedgerEntry {
    pub entry_id: String,
    pub namespace_id: String,
    pub turn_id: String,
    pub thread_id: String,
    pub kind: LedgerEntryKind,
    pub source: String,
    pub content: Option<String>,
    pub payload: Value,
    pub citation: String,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecallChunk {
    pub chunk_id: String,
    pub namespace_id: String,
    pub source_type: String,
    pub source_id: String,
    pub entry_id: String,
    pub chunk_index: i64,
    pub content: String,
    pub embedding_json: Option<String>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct RecallHit {
    pub entry_id: String,
    pub source_id: String,
    pub source_type: String,
    pub content: String,
    pub score: f64,
    pub citation: Option<String>,
}

pub fn chunk_text(text: &str, max_chars: usize) -> Vec<String> {
    if text.trim().is_empty() {
        return Vec::new();
    }
    let mut chunks = Vec::new();
    let mut current = String::new();
    for paragraph in text.split("\n\n") {
        let paragraph = paragraph.trim();
        if paragraph.is_empty() {
            continue;
        }
        if current.is_empty() {
            current.push_str(paragraph);
            continue;
        }
        if current.len() + 2 + paragraph.len() <= max_chars {
            current.push_str("\n\n");
            current.push_str(paragraph);
        } else {
            chunks.push(current);
            current = paragraph.to_string();
        }
    }
    if !current.is_empty() {
        chunks.push(current);
    }
    if chunks.is_empty() {
        vec![text.chars().take(max_chars).collect()]
    } else {
        chunks
    }
}

pub fn cosine_similarity(left: &[f32], right: &[f32]) -> Option<f64> {
    if left.len() != right.len() || left.is_empty() {
        return None;
    }
    let mut dot = 0.0f64;
    let mut left_norm = 0.0f64;
    let mut right_norm = 0.0f64;
    for (l, r) in left.iter().zip(right.iter()) {
        let l = *l as f64;
        let r = *r as f64;
        dot += l * r;
        left_norm += l * l;
        right_norm += r * r;
    }
    let denom = left_norm.sqrt() * right_norm.sqrt();
    (denom > 0.0).then_some(dot / denom)
}
