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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMessage {
    pub role: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
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
    pub temperature: Option<f32>,
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
