use serde::{Deserialize, Serialize};

use crate::model::ModelUsage;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ModelEvent {
    ExchangeStarted,
    TextDelta { text: String },
    TextSnapshot { text: String },
    TextFinal { text: String },
    ReasoningDelta { text: String },
    ReasoningSnapshot { text: String },
    ReasoningFinal { text: String },
    ToolCallStarted { key: String, id: Option<String> },
    ToolCallNameDelta { key: String, text: String },
    ToolCallArgumentsDelta { key: String, text: String },
    ToolCallFinished { key: String },
    UsageUpdated { usage: ModelUsage },
    Completed { finish_reason: Option<String> },
    Failed { message: String },
}
