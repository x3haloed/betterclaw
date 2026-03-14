use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::model::TransportKind;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceBlob {
    pub id: String,
    pub encoding: String,
    pub content_type: String,
    pub body: Vec<u8>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TraceOutcome {
    Ok,
    ParseError,
    TransportError,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawFrame {
    pub sequence: usize,
    pub data: Value,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RawModelTrace {
    pub request_body: Value,
    pub response_body: Option<Value>,
    pub raw_frames: Vec<RawFrame>,
    pub provider_request_id: Option<String>,
    pub transport_kind: TransportKind,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelTrace {
    pub id: String,
    pub turn_id: String,
    pub thread_id: String,
    pub agent_id: String,
    pub channel: String,
    pub model: String,
    pub request_started_at: DateTime<Utc>,
    pub request_completed_at: DateTime<Utc>,
    pub duration_ms: i64,
    pub outcome: TraceOutcome,
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
    pub provider_request_id: Option<String>,
    pub tool_count: i64,
    pub tool_names: Vec<String>,
    pub request_blob_id: String,
    pub response_blob_id: String,
    pub stream_blob_id: Option<String>,
    pub error_summary: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TraceDetail {
    pub trace: ModelTrace,
    pub request_body: Value,
    pub response_body: Value,
    pub stream_body: Option<Value>,
}
