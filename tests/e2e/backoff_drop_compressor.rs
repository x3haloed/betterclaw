use betterclaw::llm::{ProviderBackoff, BackoffObserverProvider, BackoffGuardProvider};
use betterclaw::error::LlmError;
use std::sync::Arc;

#[tokio::test]
async fn compressor_is_dropped_when_provider_rate_limited() {
    // Setup: create a provider that simulates a 429 with Retry-After
    let simulated = crate::test_helpers::SimulatedProvider::new_rate_limited("simprov", std::time::Duration::from_secs(5));

    let backoff = Arc::new(ProviderBackoff::new());

    // Wrap final provider with observer so backoff gets recorded
    let observed = BackoffObserverProvider::new(Box::new(simulated.clone()), backoff.clone());

    // Build a guard for compressor that drops on backoff
    let mut compressor_guard = BackoffGuardProvider::new(Box::new(observed), backoff.clone(), true);

    // First call: underlying provider returns RateLimited and observer records backoff
    let res = compressor_guard.complete(crate::test_helpers::dummy_completion_request()).await;
    match res {
        Err(LlmError::RateLimited { .. }) => {}
        other => panic!("expected RateLimited from simulated provider, got: {:?}", other),
    }

    // Second call: guard should see backoff and drop the compressor request
    let res2 = compressor_guard.complete(crate::test_helpers::dummy_completion_request()).await;
    match res2 {
        Err(LlmError::Dropped { provider, reason }) => {
            assert_eq!(provider, "simprov");
            assert!(reason.contains("backoff active"));
        }
        other => panic!("expected Dropped from guard, got: {:?}", other),
    }
}
