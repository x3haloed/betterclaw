use thiserror::Error;

#[derive(Debug, Error)]
pub enum RuntimeError {
    #[error("agent not found: {0}")]
    AgentNotFound(String),
    #[error("thread not found: {0}")]
    ThreadNotFound(String),
    #[error("tool not found: {0}")]
    ToolNotFound(String),
    #[error("workspace not found: {0}")]
    WorkspaceNotFound(String),
    #[error("invalid tool parameters for '{tool}': {reason}")]
    InvalidToolParameters { tool: String, reason: String },
    #[error("tool execution failed for '{tool}': {reason}")]
    ToolExecution { tool: String, reason: String },
    #[error("model parse failed: {0}")]
    ModelParse(String),
    #[error(transparent)]
    ModelEngine(#[from] crate::model::ModelEngineError),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}
