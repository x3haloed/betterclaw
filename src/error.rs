use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("thread not found: {0}")]
    ThreadNotFound(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("channel not found: {0}")]
    ChannelNotFound(String),
    #[error("invalid tool parameters for '{tool}': {reason}")]
    InvalidToolParameters { tool: String, reason: String },
}
