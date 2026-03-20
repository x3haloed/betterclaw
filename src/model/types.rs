use std::sync::Arc;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use thiserror::Error;

use crate::model::openai_chatcompletions::OpenAiChatCompletionsEngine;
use crate::model::openai_responses::OpenAiResponsesEngine;
use crate::model::stub::StubModelEngine;
use crate::model::{ModelEvent, RawModelTrace, TraceOutcome};

/// A content part for multi-modal messages (text + images).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ContentPart {
    Text {
        text: String,
    },
    #[serde(rename = "image_url")]
    ImageUrl {
        image_url: ImageUrl,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageUrl {
    pub url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl ContentPart {
    pub fn text(text: impl Into<String>) -> Self {
        Self::Text { text: text.into() }
    }

    pub fn image_url(url: impl Into<String>) -> Self {
        Self::ImageUrl {
            image_url: ImageUrl {
                url: url.into(),
                detail: Some("auto".to_string()),
            },
        }
    }

    pub fn image_url_with_detail(url: impl Into<String>, detail: impl Into<String>) -> Self {
        Self::ImageUrl {
            image_url: ImageUrl {
                url: url.into(),
                detail: Some(detail.into()),
            },
        }
    }
}

/// Message content that can be either a plain string or multi-part (text + images).
/// OpenAI-compatible APIs accept both forms for the `content` field.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum MessageContent {
    /// Plain text content.
    Text(String),
    /// Multi-part content array (text + image_url).
    Parts(Vec<ContentPart>),
}

impl MessageContent {
    /// Extract plain text from either variant. Images are skipped.
    pub fn text(&self) -> Option<String> {
        match self {
            Self::Text(s) if !s.is_empty() => Some(s.clone()),
            Self::Text(_) => None,
            Self::Parts(parts) => {
                let texts: Vec<&str> = parts
                    .iter()
                    .filter_map(|p| match p {
                        ContentPart::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect();
                if texts.is_empty() {
                    None
                } else {
                    Some(texts.join("\n"))
                }
            }
        }
    }

    /// Check if this content contains any image parts.
    pub fn has_images(&self) -> bool {
        match self {
            Self::Text(_) => false,
            Self::Parts(parts) => parts
                .iter()
                .any(|p| matches!(p, ContentPart::ImageUrl { .. })),
        }
    }
}

impl Default for MessageContent {
    fn default() -> Self {
        Self::Text(String::new())
    }
}

impl From<String> for MessageContent {
    fn from(s: String) -> Self {
        Self::Text(s)
    }
}

impl From<&str> for MessageContent {
    fn from(s: &str) -> Self {
        Self::Text(s.to_string())
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: String,
    /// Message content — can be a plain string or multi-part array.
    /// OpenAI-compatible APIs accept both.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<MessageContent>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_calls: Option<Vec<ModelToolCallMessage>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelToolCallMessage {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: String,
    pub function: ModelToolFunctionMessage,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelToolFunctionMessage {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelExchangeRequest {
    pub model: String,
    pub messages: Vec<ModelMessage>,
    pub tools: Vec<Value>,
    pub max_tokens: Option<u32>,
    pub stream: bool,
    pub response_format: Option<Value>,
    pub extra: Value,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct ModelUsage {
    pub input_tokens: i64,
    pub output_tokens: i64,
    pub cache_read_input_tokens: i64,
    pub cache_creation_input_tokens: i64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ReducedToolCall {
    pub id: String,
    pub name: String,
    pub arguments_json: Option<Value>,
    pub arguments_text: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelExchangeResult {
    pub model: String,
    pub request_started_at: DateTime<Utc>,
    pub request_completed_at: DateTime<Utc>,
    pub content: Option<String>,
    pub reasoning: Option<String>,
    pub tool_calls: Vec<ReducedToolCall>,
    pub usage: ModelUsage,
    pub finish_reason: Option<String>,
    pub raw_trace: RawModelTrace,
    pub normalized_events: Vec<ModelEvent>,
    pub outcome: TraceOutcome,
    pub error_summary: Option<String>,
}

#[derive(Debug, Error)]
pub enum ModelEngineError {
    #[error("rate limited: {message}")]
    RateLimited {
        message: String,
        retry_after: Option<Duration>,
        exchange: Box<ModelExchangeResult>,
    },
    #[error("transport failure: {message}")]
    TransportFailure {
        message: String,
        exchange: Box<ModelExchangeResult>,
    },
    #[error("http failure ({status}): {message}")]
    HttpFailure {
        status: u16,
        message: String,
        exchange: Box<ModelExchangeResult>,
    },
}

impl ModelEngineError {
    pub fn exchange(&self) -> &ModelExchangeResult {
        match self {
            Self::RateLimited { exchange, .. }
            | Self::TransportFailure { exchange, .. }
            | Self::HttpFailure { exchange, .. } => exchange.as_ref(),
        }
    }

    pub fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::RateLimited { retry_after, .. } => *retry_after,
            _ => None,
        }
    }
}

#[async_trait]
pub trait ModelRunner: Send + Sync {
    async fn run(
        &self,
        request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, ModelEngineError>;
}

#[derive(Clone)]
pub enum ModelEngine {
    OpenAiChatCompletions(Arc<OpenAiChatCompletionsEngine>),
    OpenAiResponses(Arc<OpenAiResponsesEngine>),
    Stub(Arc<StubModelEngine>),
}

impl ModelEngine {
    pub fn openai_chat_completions(engine: OpenAiChatCompletionsEngine) -> Self {
        Self::OpenAiChatCompletions(Arc::new(engine))
    }

    pub fn openai_responses(engine: OpenAiResponsesEngine) -> Self {
        Self::OpenAiResponses(Arc::new(engine))
    }

    pub fn stub(engine: StubModelEngine) -> Self {
        Self::Stub(Arc::new(engine))
    }

    pub fn kind_name(&self) -> &'static str {
        match self {
            Self::OpenAiChatCompletions(_) => "openai_chat_completions",
            Self::OpenAiResponses(_) => "openai_responses",
            Self::Stub(_) => "stub",
        }
    }

    pub async fn run(
        &self,
        request: ModelExchangeRequest,
    ) -> Result<ModelExchangeResult, ModelEngineError> {
        match self {
            Self::OpenAiChatCompletions(engine) => engine.run(request).await,
            Self::OpenAiResponses(engine) => engine.run(request).await,
            Self::Stub(engine) => engine.run(request).await,
        }
    }
}
