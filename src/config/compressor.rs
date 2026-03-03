use secrecy::SecretString;

use crate::config::helpers::{optional_env, parse_optional_env};
use crate::config::llm::{LlmBackend, LlmConfig, LlmTuningConfig, OpenAiCompatibleConfig};
use crate::error::ConfigError;
use crate::settings::Settings;

fn parse_extra_headers_prefixed(key: &str) -> Result<Vec<(String, String)>, ConfigError> {
    // Reuse the same `Key:Value,Key2:Value2` format as LLM_EXTRA_HEADERS.
    if let Some(val) = optional_env(key)? {
        // Keep parsing logic centralized in llm.rs.
        crate::config::llm::parse_extra_headers(&val)
    } else {
        Ok(Vec::new())
    }
}

/// Resolve the compressor LLM config.
///
/// Behavior:
/// - If no `COMPRESSOR_LLM_*` env vars are set, falls back to the main LLM config.
/// - Otherwise, builds a fixed-model OpenAI-compatible config.
pub(crate) fn resolve_compressor_llm(
    settings: &Settings,
    fallback: &LlmConfig,
) -> Result<LlmConfig, ConfigError> {
    let has_any = optional_env("COMPRESSOR_LLM_BASE_URL")?.is_some()
        || optional_env("COMPRESSOR_LLM_MODEL")?.is_some()
        || optional_env("COMPRESSOR_LLM_API_KEY")?.is_some()
        || optional_env("COMPRESSOR_LLM_EXTRA_HEADERS")?.is_some();

    if !has_any {
        return Ok(fallback.clone());
    }

    // OpenAI-compatible only (by design).
    let base_url = optional_env("COMPRESSOR_LLM_BASE_URL")?
        .or(optional_env("LLM_BASE_URL")?)
        .or(settings.openai_compatible_base_url.clone())
        .ok_or_else(|| ConfigError::MissingRequired {
            key: "COMPRESSOR_LLM_BASE_URL".to_string(),
            hint: "Set COMPRESSOR_LLM_BASE_URL (or LLM_BASE_URL) for the compressor when using an OpenAI-compatible endpoint".to_string(),
        })?;

    let api_key = optional_env("COMPRESSOR_LLM_API_KEY")?
        .or(optional_env("LLM_API_KEY")?)
        .map(SecretString::from);

    let model = optional_env("COMPRESSOR_LLM_MODEL")?
        .or(optional_env("LLM_CHEAP_MODEL")?)
        .or(optional_env("LLM_MODEL")?)
        .or(settings.selected_model.clone())
        .unwrap_or_else(|| "default".to_string());

    let extra_headers = parse_extra_headers_prefixed("COMPRESSOR_LLM_EXTRA_HEADERS")?;

    // Compressor wants stability over cleverness: no smart routing/cascade, no fallbacks.
    let tuning = LlmTuningConfig {
        cheap_model: None,
        fallback_model: None,
        max_retries: parse_optional_env("COMPRESSOR_LLM_MAX_RETRIES", 2)?,
        circuit_breaker_threshold: None,
        circuit_breaker_recovery_secs: 30,
        response_cache_enabled: false,
        response_cache_ttl_secs: 3600,
        response_cache_max_entries: 1000,
        failover_cooldown_secs: 300,
        failover_cooldown_threshold: 3,
        smart_routing_cascade: false,
    };

    Ok(LlmConfig {
        backend: LlmBackend::OpenAiCompatible,
        tuning,
        openai: None,
        anthropic: None,
        ollama: None,
        openai_compatible: Some(OpenAiCompatibleConfig {
            base_url,
            api_key,
            model,
            extra_headers,
        }),
        tinfoil: None,
    })
}
