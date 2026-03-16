use super::*;
use crate::event::EventKind;
use crate::thread::Thread;
use crate::turn::TurnStatus;
use serde_json::Value;

#[derive(Debug, Clone, serde::Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RuntimeUpdate {
    EventAdded {
        thread_id: String,
        turn_id: String,
        kind: EventKind,
        payload: Value,
    },
    TraceRecorded {
        thread_id: String,
        turn_id: String,
        trace_id: String,
        outcome: TraceOutcome,
    },
    TurnUpdated {
        thread_id: String,
        turn_id: String,
        status: TurnStatus,
        assistant_message: Option<String>,
        error: Option<String>,
    },
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct RecoveryReport {
    pub recovered_turn_count: usize,
    pub recovered_turn_ids: Vec<String>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct TracePruneReport {
    pub retention_days: u32,
    pub pruned_blob_count: usize,
    pub reclaimed_bytes: i64,
}

pub struct TurnOutcome {
    pub thread: Thread,
    pub turn_id: String,
    pub response: String,
    pub trace_id: String,
    pub status: TurnStatus,
    pub outbound_messages: Vec<String>,
}

pub(crate) struct ToolExecutionOutcome {
    pub(crate) continuation_messages: Vec<ModelMessage>,
    pub(crate) outbound_messages: Vec<String>,
    pub(crate) ask_user_question: Option<String>,
    pub(crate) final_message: Option<String>,
}

#[derive(Debug, Clone)]
pub(crate) enum ToolControl {
    Message { content: String },
    AskUser { question: String },
    FinalMessage { content: String },
}
