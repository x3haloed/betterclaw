use secrecy::SecretString;

use crate::config::helpers::{optional_env, parse_optional_env};
use crate::config::llm::{
    AnthropicDirectConfig, LlmBackend, LlmConfig, LlmTuningConfig, OllamaConfig,
    OpenAiCompatibleConfig, OpenAiDirectConfig, TinfoilConfig, parse_extra_headers,
};
use crate::error::ConfigError;
use crate::settings::Settings;

fn resolve_backend(_settings: &Settings, fallback: LlmBackend) -> Result<LlmBackend, ConfigError> {
    if let Some(b) = optional_env("COMPRESSOR_LLM_BACKEND")? {
        return b.parse().map_err(|e| ConfigError::InvalidValue {
            key: "COMPRESSOR_LLM_BACKEND".to_string(),
            message: e,
        });
    }
    // No settings key yet; compressor defaults to fallback unless explicitly configured.
    Ok(fallback)
}

fn has_any_compressor_env() -> Result<bool, ConfigError> {
    Ok(optional_env("COMPRESSOR_LLM_BACKEND")?.is_some()
        || optional_env("COMPRESSOR_LLM_BASE_URL")?.is_some()
        || optional_env("COMPRESSOR_LLM_MODEL")?.is_some()
        || optional_env("COMPRESSOR_LLM_API_KEY")?.is_some()
        || optional_env("COMPRESSOR_LLM_EXTRA_HEADERS")?.is_some()
        || optional_env("COMPRESSOR_OPENAI_API_KEY")?.is_some()
        || optional_env("COMPRESSOR_OPENAI_MODEL")?.is_some()
        || optional_env("COMPRESSOR_OPENAI_BASE_URL")?.is_some()
        || optional_env("COMPRESSOR_ANTHROPIC_API_KEY")?.is_some()
        || optional_env("COMPRESSOR_ANTHROPIC_MODEL")?.is_some()
        || optional_env("COMPRESSOR_ANTHROPIC_BASE_URL")?.is_some()
        || optional_env("COMPRESSOR_OLLAMA_BASE_URL")?.is_some()
        || optional_env("COMPRESSOR_OLLAMA_MODEL")?.is_some()
        || optional_env("COMPRESSOR_TINFOIL_API_KEY")?.is_some()
        || optional_env("COMPRESSOR_TINFOIL_MODEL")?.is_some())
}

fn compressor_tuning() -> Result<LlmTuningConfig, ConfigError> {
    // Compressor wants stability over cleverness: no smart routing/cascade, no fallbacks.
    Ok(LlmTuningConfig {
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
    })
}

/// Resolve the compressor LLM config.
///
/// Behavior:
/// - If no `COMPRESSOR_*` env vars are set, falls back to the main LLM config.
/// - Otherwise, builds a fixed-model config for the requested backend.
pub(crate) fn resolve_compressor_llm(
    settings: &Settings,
    fallback: &LlmConfig,
) -> Result<LlmConfig, ConfigError> {
    if !has_any_compressor_env()? {
        return Ok(fallback.clone());
    }

    let backend = resolve_backend(settings, fallback.backend)?;
    let tuning = compressor_tuning()?;

    // Resolve provider-specific configs based on backend.
    let openai = if backend == LlmBackend::OpenAi {
        let api_key = optional_env("COMPRESSOR_OPENAI_API_KEY")?
            .or(optional_env("OPENAI_API_KEY")?)
            .map(SecretString::from)
            .ok_or_else(|| ConfigError::MissingRequired {
                key: "COMPRESSOR_OPENAI_API_KEY".to_string(),
                hint: "Set COMPRESSOR_OPENAI_API_KEY (or OPENAI_API_KEY) when COMPRESSOR_LLM_BACKEND=openai".to_string(),
            })?;
        let model = optional_env("COMPRESSOR_OPENAI_MODEL")?
            .or(optional_env("OPENAI_MODEL")?)
            .or(settings.selected_model.clone())
            .unwrap_or_else(|| "gpt-4o".to_string());
        let base_url = optional_env("COMPRESSOR_OPENAI_BASE_URL")?.or(optional_env("OPENAI_BASE_URL")?);
        Some(OpenAiDirectConfig { api_key, model, base_url })
    } else {
        None
    };

    let anthropic = if backend == LlmBackend::Anthropic {
        let api_key = optional_env("COMPRESSOR_ANTHROPIC_API_KEY")?
            .or(optional_env("ANTHROPIC_API_KEY")?)
            .map(SecretString::from)
            .ok_or_else(|| ConfigError::MissingRequired {
                key: "COMPRESSOR_ANTHROPIC_API_KEY".to_string(),
                hint: "Set COMPRESSOR_ANTHROPIC_API_KEY (or ANTHROPIC_API_KEY) when COMPRESSOR_LLM_BACKEND=anthropic".to_string(),
            })?;
        let model = optional_env("COMPRESSOR_ANTHROPIC_MODEL")?
            .or(optional_env("ANTHROPIC_MODEL")?)
            .or(settings.selected_model.clone())
            .unwrap_or_else(|| "claude-sonnet-4-20250514".to_string());
        let base_url =
            optional_env("COMPRESSOR_ANTHROPIC_BASE_URL")?.or(optional_env("ANTHROPIC_BASE_URL")?);
        Some(AnthropicDirectConfig { api_key, model, base_url })
    } else {
        None
    };

    let ollama = if backend == LlmBackend::Ollama {
        let base_url = optional_env("COMPRESSOR_OLLAMA_BASE_URL")?
            .or(optional_env("OLLAMA_BASE_URL")?)
            .or(settings.ollama_base_url.clone())
            .unwrap_or_else(|| "http://localhost:11434".to_string());
        let model = optional_env("COMPRESSOR_OLLAMA_MODEL")?
            .or(optional_env("OLLAMA_MODEL")?)
            .or(settings.selected_model.clone())
            .unwrap_or_else(|| "llama3".to_string());
        Some(OllamaConfig { base_url, model })
    } else {
        None
    };

    let openai_compatible = if backend == LlmBackend::OpenAiCompatible {
        let base_url = optional_env("COMPRESSOR_LLM_BASE_URL")?
            .or(optional_env("LLM_BASE_URL")?)
            .or(settings.openai_compatible_base_url.clone())
            .ok_or_else(|| ConfigError::MissingRequired {
                key: "COMPRESSOR_LLM_BASE_URL".to_string(),
                hint: "Set COMPRESSOR_LLM_BASE_URL (or LLM_BASE_URL) when COMPRESSOR_LLM_BACKEND=openai_compatible".to_string(),
            })?;
        let api_key = optional_env("COMPRESSOR_LLM_API_KEY")?
            .or(optional_env("LLM_API_KEY")?)
            .map(SecretString::from);
        let model = optional_env("COMPRESSOR_LLM_MODEL")?
            .or(optional_env("LLM_MODEL")?)
            .or(settings.selected_model.clone())
            .unwrap_or_else(|| "default".to_string());
        let extra_headers = optional_env("COMPRESSOR_LLM_EXTRA_HEADERS")?
            .map(|val| parse_extra_headers(&val))
            .transpose()?
            .unwrap_or_default();
        Some(OpenAiCompatibleConfig {
            base_url,
            api_key,
            model,
            extra_headers,
        })
    } else {
        None
    };

    let tinfoil = if backend == LlmBackend::Tinfoil {
        let api_key = optional_env("COMPRESSOR_TINFOIL_API_KEY")?
            .or(optional_env("TINFOIL_API_KEY")?)
            .map(SecretString::from)
            .ok_or_else(|| ConfigError::MissingRequired {
                key: "COMPRESSOR_TINFOIL_API_KEY".to_string(),
                hint: "Set COMPRESSOR_TINFOIL_API_KEY (or TINFOIL_API_KEY) when COMPRESSOR_LLM_BACKEND=tinfoil".to_string(),
            })?;
        let model = optional_env("COMPRESSOR_TINFOIL_MODEL")?
            .or(optional_env("TINFOIL_MODEL")?)
            .or(settings.selected_model.clone())
            .unwrap_or_else(|| "kimi-k2-5".to_string());
        Some(TinfoilConfig { api_key, model })
    } else {
        None
    };

    Ok(LlmConfig {
        backend,
        tuning,
        openai,
        anthropic,
        ollama,
        openai_compatible,
        tinfoil,
    })
}
