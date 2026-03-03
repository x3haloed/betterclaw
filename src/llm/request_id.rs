//! LLM provider wrapper that injects a per-request correlation ID and logs start/end.
//!
//! Motivation: some tracing spans (e.g. `chat{gen_ai...}`) may be emitted multiple times
//! per real outbound request. A stable `llm_request_id` makes it unambiguous whether
//! the system actually sent multiple requests or just logged a span multiple times.

use std::sync::Arc;

use async_trait::async_trait;
use uuid::Uuid;

use crate::error::LlmError;
use crate::llm::{
    CompletionRequest, CompletionResponse, LlmProvider, ModelMetadata, ToolCompletionRequest,
    ToolCompletionResponse,
};

const META_KEY: &str = "llm_request_id";

fn ensure_request_id(meta: &mut std::collections::HashMap<String, String>) -> Uuid {
    if let Some(v) = meta.get(META_KEY) {
        if let Ok(id) = Uuid::parse_str(v) {
            return id;
        }
    }
    let id = Uuid::new_v4();
    meta.insert(META_KEY.to_string(), id.to_string());
    id
}

/// Wrapper provider that guarantees `llm_request_id` in request metadata and logs start/end.
pub struct RequestIdProvider {
    inner: Arc<dyn LlmProvider>,
}

impl RequestIdProvider {
    pub fn new(inner: Arc<dyn LlmProvider>) -> Self {
        Self { inner }
    }
}

#[async_trait]
impl LlmProvider for RequestIdProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        self.inner.cost_per_token()
    }

    async fn complete(&self, mut request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let llm_request_id = ensure_request_id(&mut request.metadata);
        let model = self
            .inner
            .effective_model_name(request.model.as_deref());
        tracing::info!(
            llm_request_id = %llm_request_id,
            model = %model,
            "LLM request start"
        );
        let resp = self.inner.complete(request).await?;
        tracing::info!(
            llm_request_id = %llm_request_id,
            model = %model,
            input_tokens = resp.input_tokens,
            output_tokens = resp.output_tokens,
            "LLM request end"
        );
        Ok(resp)
    }

    async fn complete_with_tools(
        &self,
        mut request: ToolCompletionRequest,
    ) -> Result<ToolCompletionResponse, LlmError> {
        let llm_request_id = ensure_request_id(&mut request.metadata);
        let model = self
            .inner
            .effective_model_name(request.model.as_deref());
        tracing::info!(
            llm_request_id = %llm_request_id,
            model = %model,
            tools = request.tools.len(),
            tool_choice = ?request.tool_choice,
            "LLM request start (tools)"
        );
        let resp = self.inner.complete_with_tools(request).await?;
        tracing::info!(
            llm_request_id = %llm_request_id,
            model = %model,
            input_tokens = resp.input_tokens,
            output_tokens = resp.output_tokens,
            finish_reason = ?resp.finish_reason,
            tool_calls = resp.tool_calls.len(),
            "LLM request end (tools)"
        );
        Ok(resp)
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.inner.list_models().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        self.inner.model_metadata().await
    }

    fn effective_model_name(&self, requested_model: Option<&str>) -> String {
        self.inner.effective_model_name(requested_model)
    }

    fn active_model_name(&self) -> String {
        self.inner.active_model_name()
    }

    fn set_model(&self, model: &str) -> Result<(), LlmError> {
        self.inner.set_model(model)
    }

    fn calculate_cost(&self, input_tokens: u32, output_tokens: u32) -> rust_decimal::Decimal {
        self.inner.calculate_cost(input_tokens, output_tokens)
    }
}

