//! LLM integration for the agent.
//!
//! Supports multiple backends:
//! - **OpenAI**: Direct API access with your own key
//! - **Anthropic**: Direct API access with your own key
//! - **Ollama**: Local model inference
//! - **OpenAI-compatible**: Any endpoint that speaks the OpenAI API
//! - **OpenAI Codex**: OpenAI API using Codex auth from ~/.codex/auth.json

pub mod circuit_breaker;
pub mod costs;
pub mod failover;
mod openai_codex;
mod provider;
mod reasoning;
mod request_id;
pub mod response_cache;
pub mod retry;
mod rig_adapter;
pub mod smart_routing;

pub use circuit_breaker::{CircuitBreakerConfig, CircuitBreakerProvider};
pub use failover::{CooldownConfig, FailoverProvider};
pub use provider::{
    ChatMessage, CompletionRequest, CompletionResponse, FinishReason, LlmProvider, ModelMetadata,
    Role, ToolCall, ToolCompletionRequest, ToolCompletionResponse, ToolDefinition, ToolResult,
};
pub use reasoning::{
    ActionPlan, Reasoning, ReasoningContext, RespondOutput, RespondResult, SILENT_REPLY_TOKEN,
    TokenUsage, ToolSelection, is_silent_reply,
};
pub use request_id::RequestIdProvider;
pub use response_cache::{CachedProvider, ResponseCacheConfig};
pub use retry::{RetryConfig, RetryProvider};
pub use rig_adapter::RigAdapter;
pub use smart_routing::{SmartRoutingConfig, SmartRoutingProvider, TaskComplexity};

use std::sync::Arc;

use rig::client::CompletionClient;
use secrecy::ExposeSecret;

use crate::config::{LlmBackend, LlmConfig};
use crate::error::LlmError;

/// Create an LLM provider based on configuration.
///
/// - Other backends: Use rig-core adapter with provider-specific clients
pub fn create_llm_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let provider = match config.backend {
        LlmBackend::OpenAi => create_openai_provider(config),
        LlmBackend::Anthropic => create_anthropic_provider(config),
        LlmBackend::Ollama => create_ollama_provider(config),
        LlmBackend::OpenAiCompatible => create_openai_compatible_provider(config),
        LlmBackend::OpenAiCodex => create_openai_codex_provider(config),
        LlmBackend::Tinfoil => create_tinfoil_provider(config),
    }?;
    Ok(Arc::new(RequestIdProvider::new(provider)))
}

fn create_llm_provider_with_model(
    config: &LlmConfig,
    model_override: &str,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let provider = match config.backend {
        LlmBackend::OpenAi => {
            let mut c = config
                .openai
                .as_ref()
                .ok_or_else(|| LlmError::AuthFailed {
                    provider: "openai".to_string(),
                })?
                .clone();
            c.model = model_override.to_string();
            create_openai_provider_from(&c)
        }
        LlmBackend::Anthropic => {
            let mut c = config
                .anthropic
                .as_ref()
                .ok_or_else(|| LlmError::AuthFailed {
                    provider: "anthropic".to_string(),
                })?
                .clone();
            c.model = model_override.to_string();
            create_anthropic_provider_from(&c)
        }
        LlmBackend::Ollama => {
            let mut c = config
                .ollama
                .as_ref()
                .ok_or_else(|| LlmError::AuthFailed {
                    provider: "ollama".to_string(),
                })?
                .clone();
            c.model = model_override.to_string();
            create_ollama_provider_from(&c)
        }
        LlmBackend::OpenAiCompatible => {
            let mut c = config
                .openai_compatible
                .as_ref()
                .ok_or_else(|| LlmError::AuthFailed {
                    provider: "openai_compatible".to_string(),
                })?
                .clone();
            c.model = model_override.to_string();
            create_openai_compatible_provider_from(&c)
        }
        LlmBackend::OpenAiCodex => {
            let mut c = config
                .openai_codex
                .as_ref()
                .ok_or_else(|| LlmError::AuthFailed {
                    provider: "openai_codex".to_string(),
                })?
                .clone();
            c.model = model_override.to_string();
            create_openai_codex_provider_from(&c)
        }
        LlmBackend::Tinfoil => {
            let mut c = config
                .tinfoil
                .as_ref()
                .ok_or_else(|| LlmError::AuthFailed {
                    provider: "tinfoil".to_string(),
                })?
                .clone();
            c.model = model_override.to_string();
            create_tinfoil_provider_from(&c)
        }
    }?;
    Ok(Arc::new(RequestIdProvider::new(provider)))
}

fn create_openai_codex_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let codex = config
        .openai_codex
        .as_ref()
        .ok_or_else(|| LlmError::AuthFailed {
            provider: "openai_codex".to_string(),
        })?;
    create_openai_codex_provider_from(codex)
}

fn create_openai_codex_provider_from(
    codex: &crate::config::OpenAiCodexConfig,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    tracing::info!(
        "Using OpenAI Codex mode (base_url: {}, model: {}, auth_file: {})",
        codex.base_url,
        codex.model,
        codex.auth_file
    );
    Ok(Arc::new(openai_codex::OpenAiCodexProvider::new(codex)?))
}

fn create_openai_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let oai = config.openai.as_ref().ok_or_else(|| LlmError::AuthFailed {
        provider: "openai".to_string(),
    })?;
    create_openai_provider_from(oai)
}

fn create_openai_provider_from(
    oai: &crate::config::OpenAiDirectConfig,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    use rig::providers::openai;

    // Use CompletionsClient (Chat Completions API) instead of the default Client
    // (Responses API). The Responses API path in rig-core panics when tool results
    // are sent back because betterclaw doesn't thread `call_id` through its ToolCall
    // type. The Chat Completions API works correctly with the existing code.
    let client: openai::CompletionsClient = if let Some(ref base_url) = oai.base_url {
        tracing::info!(
            "Using OpenAI direct API (chat completions, model: {}, base_url: {})",
            oai.model,
            base_url,
        );
        openai::Client::builder()
            .base_url(base_url)
            .api_key(oai.api_key.expose_secret())
            .build()
    } else {
        tracing::info!(
            "Using OpenAI direct API (chat completions, model: {}, base_url: default)",
            oai.model,
        );
        openai::Client::new(oai.api_key.expose_secret())
    }
    .map_err(|e| LlmError::RequestFailed {
        provider: "openai".to_string(),
        reason: format!("Failed to create OpenAI client: {}", e),
    })?
    .completions_api();

    let model = client.completion_model(&oai.model);
    Ok(Arc::new(RigAdapter::new(model, &oai.model)))
}

fn create_anthropic_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let anth = config
        .anthropic
        .as_ref()
        .ok_or_else(|| LlmError::AuthFailed {
            provider: "anthropic".to_string(),
        })?;

    create_anthropic_provider_from(anth)
}

fn create_anthropic_provider_from(
    anth: &crate::config::AnthropicDirectConfig,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    use rig::providers::anthropic;

    let client: anthropic::Client = if let Some(ref base_url) = anth.base_url {
        anthropic::Client::builder()
            .api_key(anth.api_key.expose_secret())
            .base_url(base_url)
            .build()
    } else {
        anthropic::Client::new(anth.api_key.expose_secret())
    }
    .map_err(|e| LlmError::RequestFailed {
        provider: "anthropic".to_string(),
        reason: format!("Failed to create Anthropic client: {}", e),
    })?;

    let model = client.completion_model(&anth.model);
    tracing::info!(
        "Using Anthropic direct API (model: {}, base_url: {})",
        anth.model,
        anth.base_url.as_deref().unwrap_or("default"),
    );
    Ok(Arc::new(RigAdapter::new(model, &anth.model)))
}

fn create_ollama_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let oll = config.ollama.as_ref().ok_or_else(|| LlmError::AuthFailed {
        provider: "ollama".to_string(),
    })?;
    create_ollama_provider_from(oll)
}

fn create_ollama_provider_from(
    oll: &crate::config::OllamaConfig,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    use rig::client::Nothing;
    use rig::providers::ollama;

    let client: ollama::Client = ollama::Client::builder()
        .base_url(&oll.base_url)
        .api_key(Nothing)
        .build()
        .map_err(|e| LlmError::RequestFailed {
            provider: "ollama".to_string(),
            reason: format!("Failed to create Ollama client: {}", e),
        })?;

    let model = client.completion_model(&oll.model);
    tracing::info!(
        "Using Ollama (base_url: {}, model: {})",
        oll.base_url,
        oll.model
    );
    Ok(Arc::new(RigAdapter::new(model, &oll.model)))
}

const TINFOIL_BASE_URL: &str = "https://inference.tinfoil.sh/v1";

fn create_tinfoil_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let tf = config
        .tinfoil
        .as_ref()
        .ok_or_else(|| LlmError::AuthFailed {
            provider: "tinfoil".to_string(),
        })?;

    create_tinfoil_provider_from(tf)
}

fn create_tinfoil_provider_from(
    tf: &crate::config::TinfoilConfig,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    use rig::providers::openai;

    let client: openai::Client = openai::Client::builder()
        .base_url(TINFOIL_BASE_URL)
        .api_key(tf.api_key.expose_secret())
        .build()
        .map_err(|e| LlmError::RequestFailed {
            provider: "tinfoil".to_string(),
            reason: format!("Failed to create Tinfoil client: {}", e),
        })?;

    // Tinfoil currently only supports the Chat Completions API and not the newer Responses API,
    // so we must explicitly select the completions API here (unlike other OpenAI-compatible providers).
    let client = client.completions_api();
    let model = client.completion_model(&tf.model);
    tracing::info!("Using Tinfoil private inference (model: {})", tf.model);
    Ok(Arc::new(RigAdapter::new(model, &tf.model)))
}

fn create_openai_compatible_provider(config: &LlmConfig) -> Result<Arc<dyn LlmProvider>, LlmError> {
    let compat = config
        .openai_compatible
        .as_ref()
        .ok_or_else(|| LlmError::AuthFailed {
            provider: "openai_compatible".to_string(),
        })?;

    create_openai_compatible_provider_from(compat)
}

fn create_openai_compatible_provider_from(
    compat: &crate::config::OpenAiCompatibleConfig,
) -> Result<Arc<dyn LlmProvider>, LlmError> {
    // Note: OpenRouter is "OpenAI-compatible" at the HTTP level, but its error
    // responses and some edge cases differ enough that rig-core ships a dedicated
    // OpenRouter provider. Using it avoids JSON decode failures for OpenRouter
    // error shapes (e.g. `{ "message": "..." }`).
    let base_url_lc = compat.base_url.to_lowercase();
    let is_openrouter = base_url_lc.contains("openrouter.ai");

    let mut extra_headers = reqwest::header::HeaderMap::new();
    for (key, value) in &compat.extra_headers {
        let name = match reqwest::header::HeaderName::from_bytes(key.as_bytes()) {
            Ok(n) => n,
            Err(e) => {
                tracing::warn!(header = %key, error = %e, "Skipping LLM_EXTRA_HEADERS entry: invalid header name");
                continue;
            }
        };
        let val = match reqwest::header::HeaderValue::from_str(value) {
            Ok(v) => v,
            Err(e) => {
                tracing::warn!(header = %key, error = %e, "Skipping LLM_EXTRA_HEADERS entry: invalid header value");
                continue;
            }
        };
        extra_headers.insert(name, val);
    }

    let api_key = compat
        .api_key
        .as_ref()
        .map(|k| k.expose_secret().to_string())
        .unwrap_or_else(|| "no-key".to_string());

    if is_openrouter {
        use rig::providers::openrouter;
        let client: openrouter::Client = openrouter::Client::builder()
            // Allow overrides (tests/local proxies), but default is OpenRouter's canonical base.
            .base_url(&compat.base_url)
            .api_key(&api_key)
            .http_headers(extra_headers)
            .build()
            .map_err(|e| LlmError::RequestFailed {
                provider: "openrouter".to_string(),
                reason: format!("Failed to create OpenRouter client: {}", e),
            })?;
        let model = client.completion_model(&compat.model);
        tracing::info!(
            "Using OpenRouter endpoint (rig openrouter provider, base_url: {}, model: {})",
            compat.base_url,
            compat.model
        );
        Ok(Arc::new(RigAdapter::new(model, &compat.model)))
    } else {
        use rig::providers::openai;
        let client: openai::CompletionsClient = openai::Client::builder()
            .base_url(&compat.base_url)
            .api_key(api_key)
            .http_headers(extra_headers)
            .build()
            .map_err(|e| LlmError::RequestFailed {
                provider: "openai_compatible".to_string(),
                reason: format!("Failed to create OpenAI-compatible client: {}", e),
            })?
            .completions_api();

        let model = client.completion_model(&compat.model);
        tracing::info!(
            "Using OpenAI-compatible endpoint (chat completions, base_url: {}, model: {})",
            compat.base_url,
            compat.model
        );
        Ok(Arc::new(RigAdapter::new(model, &compat.model)))
    }
}

/// Create a cheap/fast LLM provider for lightweight tasks (heartbeat, routing, evaluation).
///
/// Uses `LLM_CHEAP_MODEL` (via config/env) when set, otherwise returns `None`.
pub fn create_cheap_llm_provider(
    config: &LlmConfig,
) -> Result<Option<Arc<dyn LlmProvider>>, LlmError> {
    let Some(ref cheap_model) = config.tuning.cheap_model else {
        return Ok(None);
    };
    Ok(Some(create_llm_provider_with_model(config, cheap_model)?))
}

/// Build the full LLM provider chain with all configured wrappers.
///
/// Applies decorators in this order:
/// 1. Raw provider (from config)
/// 2. RetryProvider (per-provider retry with exponential backoff)
/// 3. SmartRoutingProvider (cheap/primary split when cheap model is configured)
/// 4. FailoverProvider (fallback model when primary fails)
/// 5. CircuitBreakerProvider (fast-fail when backend is degraded)
/// 6. CachedProvider (in-memory response cache)
///
/// Also returns a separate cheap LLM provider for heartbeat/evaluation (not
/// part of the chain — it's a standalone provider for explicitly cheap tasks).
///
/// This is the single source of truth for provider chain construction,
/// called by both `main.rs` and `app.rs`.
#[allow(clippy::type_complexity)]
pub fn build_provider_chain(
    config: &LlmConfig,
) -> Result<(Arc<dyn LlmProvider>, Option<Arc<dyn LlmProvider>>), LlmError> {
    let llm = create_llm_provider(config)?;
    tracing::info!("LLM provider initialized: {}", llm.model_name());

    // 1. Retry
    let retry_config = RetryConfig {
        max_retries: config.tuning.max_retries,
    };
    let llm: Arc<dyn LlmProvider> = if retry_config.max_retries > 0 {
        tracing::info!(
            max_retries = retry_config.max_retries,
            "LLM retry wrapper enabled"
        );
        Arc::new(RetryProvider::new(llm, retry_config.clone()))
    } else {
        llm
    };

    // 2. Smart routing (cheap/primary split)
    let llm: Arc<dyn LlmProvider> = if let Some(ref cheap_model) = config.tuning.cheap_model {
        let cheap = create_llm_provider_with_model(config, cheap_model)?;
        let cheap: Arc<dyn LlmProvider> = if retry_config.max_retries > 0 {
            Arc::new(RetryProvider::new(cheap, retry_config.clone()))
        } else {
            cheap
        };
        tracing::info!(
            primary = %llm.model_name(),
            cheap = %cheap.model_name(),
            "Smart routing enabled"
        );
        Arc::new(SmartRoutingProvider::new(
            llm,
            cheap,
            SmartRoutingConfig {
                cascade_enabled: config.tuning.smart_routing_cascade,
                ..SmartRoutingConfig::default()
            },
        ))
    } else {
        llm
    };

    // 3. Failover
    let llm: Arc<dyn LlmProvider> = if let Some(ref fallback_model) = config.tuning.fallback_model {
        let fallback = create_llm_provider_with_model(config, fallback_model)?;
        tracing::info!(
            primary = %llm.model_name(),
            fallback = %fallback.model_name(),
            "LLM failover enabled"
        );
        let fallback: Arc<dyn LlmProvider> = if retry_config.max_retries > 0 {
            Arc::new(RetryProvider::new(fallback, retry_config.clone()))
        } else {
            fallback
        };
        let cooldown_config = CooldownConfig {
            cooldown_duration: std::time::Duration::from_secs(config.tuning.failover_cooldown_secs),
            failure_threshold: config.tuning.failover_cooldown_threshold,
        };
        Arc::new(FailoverProvider::with_cooldown(
            vec![llm, fallback],
            cooldown_config,
        )?)
    } else {
        llm
    };

    // 4. Circuit breaker
    let llm: Arc<dyn LlmProvider> = if let Some(threshold) = config.tuning.circuit_breaker_threshold
    {
        let cb_config = CircuitBreakerConfig {
            failure_threshold: threshold,
            recovery_timeout: std::time::Duration::from_secs(
                config.tuning.circuit_breaker_recovery_secs,
            ),
            ..CircuitBreakerConfig::default()
        };
        tracing::info!(
            threshold,
            recovery_secs = config.tuning.circuit_breaker_recovery_secs,
            "LLM circuit breaker enabled"
        );
        Arc::new(CircuitBreakerProvider::new(llm, cb_config))
    } else {
        llm
    };

    // 5. Response cache
    let llm: Arc<dyn LlmProvider> = if config.tuning.response_cache_enabled {
        let rc_config = ResponseCacheConfig {
            ttl: std::time::Duration::from_secs(config.tuning.response_cache_ttl_secs),
            max_entries: config.tuning.response_cache_max_entries,
        };
        tracing::info!(
            ttl_secs = config.tuning.response_cache_ttl_secs,
            max_entries = config.tuning.response_cache_max_entries,
            "LLM response cache enabled"
        );
        Arc::new(CachedProvider::new(llm, rc_config))
    } else {
        llm
    };

    // Standalone cheap LLM for heartbeat/evaluation (not part of the chain)
    let cheap_llm = create_cheap_llm_provider(config)?;
    if let Some(ref cheap) = cheap_llm {
        tracing::info!("Cheap LLM provider initialized: {}", cheap.model_name());
    }

    Ok((llm, cheap_llm))
}

// (No llm module unit tests yet; most logic is exercised by higher-level tests.)
