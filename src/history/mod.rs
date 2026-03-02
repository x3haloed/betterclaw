//! History and persistence layer.
//!
//! Stores job history, conversations, and actions in the database for:
//! - Audit trail
//! - Learning from past executions
//! - Analytics and metrics

mod store;

pub use store::{
    AgentJobRecord, AgentJobSummary, ConversationMessage, ConversationSummary, JobEventRecord,
    LlmCallRecord, SandboxJobRecord, SandboxJobSummary, SettingRow,
};
