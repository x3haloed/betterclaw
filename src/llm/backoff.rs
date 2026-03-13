use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use tokio::sync::RwLock;

use crate::error::LlmError;
use crate::llm::provider::{CompletionRequest, CompletionResponse, LlmProvider, ToolCompletionRequest, ToolCompletionResponse, ModelMetadata};

/// Shared backoff registry keyed by provider name.
#[derive(Debug, Default)]
pub struct ProviderBackoff {
    inner: RwLock<HashMap<String, Instant>>,
}

impl ProviderBackoff {
    pub fn new() -> Self {
        Self {
            inner: RwLock::new(HashMap::new()),
        }
    }

    /// Set backoff for `provider` for `duration` from now.
    pub async fn set_backoff(&self, provider: &str, duration: Duration) {
        let mut m = self.inner.write().await;
        m.insert(provider.to_string(), Instant::now() + duration);
    }

    /// Get remaining backoff duration for `provider`, if any.
    pub async fn get_remaining(&self, provider: &str) -> Option<Duration> {
        let now = Instant::now();
        let deadline = {
            let m = self.inner.read().await;
            m.get(provider).copied()
        };

        match deadline {
            Some(deadline) if deadline > now => Some(deadline - now),
            Some(_) => {
                let mut m = self.inner.write().await;
                if m.get(provider).is_some_and(|deadline| Instant::now() >= *deadline) {
                    m.remove(provider);
                }
                None
            }
            None => None,
        }
    }
}

/// Wrapper that observes `RateLimited` responses and records the provider-suggested backoff.
pub struct BackoffObserverProvider {
    inner: Arc<dyn LlmProvider>,
    backoff: Arc<ProviderBackoff>,
}

impl BackoffObserverProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, backoff: Arc<ProviderBackoff>) -> Self {
        Self { inner, backoff }
    }
}

#[async_trait]
impl LlmProvider for BackoffObserverProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        self.inner.cost_per_token()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        match self.inner.complete(request).await {
            Ok(r) => Ok(r),
            Err(err) => {
                if let LlmError::RateLimited { retry_after: Some(d), .. } = &err {
                    self.backoff.set_backoff(self.inner.model_name(), *d).await;
                }
                Err(err)
            }
        }
    }

    async fn complete_with_tools(&self, request: ToolCompletionRequest) -> Result<ToolCompletionResponse, LlmError> {
        match self.inner.complete_with_tools(request).await {
            Ok(r) => Ok(r),
            Err(err) => {
                if let LlmError::RateLimited { retry_after: Some(d), .. } = &err {
                    self.backoff.set_backoff(self.inner.model_name(), *d).await;
                }
                Err(err)
            }
        }
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.inner.list_models().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        self.inner.model_metadata().await
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

/// Optional pre-flight backoff provider that spaces requests to a provider
/// by ensuring at least `min_interval` between consecutive requests.
pub struct PreflightBackoffProvider {
    inner: Arc<dyn LlmProvider>,
    min_interval: Duration,
    // last request timestamp per provider name
    last: RwLock<HashMap<String, Instant>>,
}

impl PreflightBackoffProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, min_interval: Duration) -> Self {
        Self {
            inner,
            min_interval,
            last: RwLock::new(HashMap::new()),
        }
    }
}

#[async_trait]
impl LlmProvider for PreflightBackoffProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        self.inner.cost_per_token()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        let name = self.inner.model_name().to_string();
        // Atomic check-and-reserve: acquire write lock, inspect deadline, sleep
        // if another caller holds the slot, then retry. This prevents two
        // concurrent callers from both observing a stale state and proceeding
        // without spacing.
        loop {
            // Acquire write lock to both inspect and reserve the slot.
            let mut m = self.last.write().await;
            if let Some(deadline) = m.get(&name) {
                let now = Instant::now();
                if *deadline > now {
                    let remaining = *deadline - now;
                    // Drop lock before sleeping so other tasks can observe the
                    // active deadline and avoid busy-waiting.
                    drop(m);
                    tracing::info!(provider=%name, wait_ms=%remaining.as_millis(), "Preflight backoff delaying request");
                    tokio::time::sleep(remaining).await;
                    // After sleeping, loop and try to reserve again.
                    continue;
                }
            }
            // No active deadline — reserve the slot and proceed.
            let next = Instant::now() + self.min_interval;
            m.insert(name.clone(), next);
            break;
        }

        self.inner.complete(request).await
    }

    async fn complete_with_tools(&self, request: ToolCompletionRequest) -> Result<ToolCompletionResponse, LlmError> {
        let name = self.inner.model_name().to_string();
        loop {
            let mut m = self.last.write().await;
            if let Some(deadline) = m.get(&name) {
                let now = Instant::now();
                if *deadline > now {
                    let remaining = *deadline - now;
                    drop(m);
                    tracing::info!(provider=%name, wait_ms=%remaining.as_millis(), "Preflight backoff delaying tool request");
                    tokio::time::sleep(remaining).await;
                    continue;
                }
            }
            let next = Instant::now() + self.min_interval;
            m.insert(name.clone(), next);
            break;
        }

        self.inner.complete_with_tools(request).await
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.inner.list_models().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        self.inner.model_metadata().await
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

/// Guard wrapper that checks provider-level backoff before forwarding calls.
pub struct BackoffGuardProvider {
    inner: Arc<dyn LlmProvider>,
    backoff: Arc<ProviderBackoff>,
    drop_on_backoff: bool,
}

impl BackoffGuardProvider {
    pub fn new(inner: Arc<dyn LlmProvider>, backoff: Arc<ProviderBackoff>, drop_on_backoff: bool) -> Self {
        Self { inner, backoff, drop_on_backoff }
    }
}

#[async_trait]
impl LlmProvider for BackoffGuardProvider {
    fn model_name(&self) -> &str {
        self.inner.model_name()
    }

    fn cost_per_token(&self) -> (rust_decimal::Decimal, rust_decimal::Decimal) {
        self.inner.cost_per_token()
    }

    async fn complete(&self, request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
        if let Some(remaining) = self.backoff.get_remaining(self.inner.model_name()).await {
            if self.drop_on_backoff {
                return Err(LlmError::Dropped { provider: self.inner.model_name().to_string(), reason: format!("backoff active for {:.0}s", remaining.as_secs_f64()) });
            } else {
                return Err(LlmError::RateLimited { provider: self.inner.model_name().to_string(), retry_after: Some(remaining) });
            }
        }
        self.inner.complete(request).await
    }

    async fn complete_with_tools(&self, request: ToolCompletionRequest) -> Result<ToolCompletionResponse, LlmError> {
        if let Some(remaining) = self.backoff.get_remaining(self.inner.model_name()).await {
            if self.drop_on_backoff {
                return Err(LlmError::Dropped { provider: self.inner.model_name().to_string(), reason: format!("backoff active for {:.0}s", remaining.as_secs_f64()) });
            } else {
                return Err(LlmError::RateLimited { provider: self.inner.model_name().to_string(), retry_after: Some(remaining) });
            }
        }
        self.inner.complete_with_tools(request).await
    }

    async fn list_models(&self) -> Result<Vec<String>, LlmError> {
        self.inner.list_models().await
    }

    async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> {
        self.inner.model_metadata().await
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

#[cfg(test)]
mod tests {
    use super::*;
    use async_trait::async_trait;
    use rust_decimal::Decimal;

    struct FakeRateLimited;

    #[async_trait]
    impl LlmProvider for FakeRateLimited {
        fn model_name(&self) -> &str { "fake" }
        fn cost_per_token(&self) -> (Decimal, Decimal) { (Decimal::ZERO, Decimal::ZERO) }
        async fn complete(&self, _request: CompletionRequest) -> Result<CompletionResponse, LlmError> {
            Err(LlmError::RateLimited { provider: "fake".to_string(), retry_after: Some(Duration::from_secs(2)) })
        }
        async fn complete_with_tools(&self, _request: ToolCompletionRequest) -> Result<ToolCompletionResponse, LlmError> {
            Err(LlmError::RateLimited { provider: "fake".to_string(), retry_after: Some(Duration::from_secs(2)) })
        }
        async fn list_models(&self) -> Result<Vec<String>, LlmError> { Ok(vec![]) }
        async fn model_metadata(&self) -> Result<ModelMetadata, LlmError> { Ok(ModelMetadata { id: "fake".to_string(), context_length: None }) }
        fn active_model_name(&self) -> String { "fake".to_string() }
        fn set_model(&self, _model: &str) -> Result<(), LlmError> { Ok(()) }
        fn calculate_cost(&self, _i: u32, _o: u32) -> Decimal { Decimal::ZERO }
    }

    #[tokio::test]
    async fn observer_sets_backoff() {
        let backoff = Arc::new(ProviderBackoff::new());
        let inner = Arc::new(FakeRateLimited);
        let obs = BackoffObserverProvider::new(inner, Arc::clone(&backoff));

        let _ = obs.complete(crate::llm::CompletionRequest::new(vec![])).await;
        let rem = backoff.get_remaining("fake").await;
        assert!(rem.is_some(), "backoff should be set");
    }

    #[tokio::test]
    async fn guard_blocks_when_backoff_set() {
        let backoff = Arc::new(ProviderBackoff::new());
        backoff.set_backoff("fake", Duration::from_secs(5)).await;
        let inner = Arc::new(FakeRateLimited);
        let guard = BackoffGuardProvider::new(inner, Arc::clone(&backoff), true);

        let err = guard.complete(crate::llm::CompletionRequest::new(vec![])).await.unwrap_err();
        match err {
            LlmError::Dropped { provider, reason } => {
                assert_eq!(provider, "fake");
                assert!(reason.contains("backoff active"));
            }
            _ => panic!("expected Dropped"),
        }
    }

    #[tokio::test]
    async fn get_remaining_removes_expired_entries() {
        let backoff = ProviderBackoff::new();
        backoff.set_backoff("fake", Duration::from_millis(1)).await;

        tokio::time::sleep(Duration::from_millis(10)).await;

        assert!(backoff.get_remaining("fake").await.is_none());

        let inner = backoff.inner.read().await;
        assert!(
            !inner.contains_key("fake"),
            "expired provider entries should be cleaned up when observed"
        );
    }
}
