use std::sync::Arc;

use secrecy::{ExposeSecret, SecretString};

use crate::config::helpers::{optional_env, parse_bool_env, parse_optional_env};
use crate::error::ConfigError;
use crate::settings::Settings;
use crate::workspace::EmbeddingProvider;

/// Embeddings provider configuration.
#[derive(Debug, Clone)]
pub struct EmbeddingsConfig {
    /// Whether embeddings are enabled.
    pub enabled: bool,
    /// Provider to use: "openai", "openai_compatible", or "ollama"
    pub provider: String,
    /// OpenAI API key (for OpenAI provider).
    pub openai_api_key: Option<SecretString>,
    /// OpenAI-compatible base URL (for openai_compatible provider).
    pub openai_compatible_base_url: Option<String>,
    /// OpenAI-compatible API key (optional, for openai_compatible provider).
    pub openai_compatible_api_key: Option<SecretString>,
    /// Model to use for embeddings.
    pub model: String,
    /// Ollama base URL (for Ollama provider). Defaults to http://localhost:11434.
    pub ollama_base_url: String,
    /// Embedding vector dimension. Inferred from the model name when not set explicitly.
    pub dimension: usize,
    /// Max characters per embedding input string (hard clamp before sending).
    pub max_input_chars: usize,
    /// Max total characters across a single embeddings request (batch splitter).
    pub max_batch_chars: usize,
}

impl Default for EmbeddingsConfig {
    fn default() -> Self {
        let model = "text-embedding-3-small".to_string();
        let dimension = default_dimension_for_model(&model);
        let max_input_chars = default_max_input_chars_for_model(&model);
        let max_batch_chars = 32_000;
        Self {
            enabled: false,
            provider: "openai".to_string(),
            openai_api_key: None,
            openai_compatible_base_url: None,
            openai_compatible_api_key: None,
            model,
            ollama_base_url: "http://localhost:11434".to_string(),
            dimension,
            max_input_chars,
            max_batch_chars,
        }
    }
}

/// Infer the embedding dimension from a well-known model name.
///
/// Falls back to 1536 (OpenAI text-embedding-3-small default) for unknown models.
fn default_dimension_for_model(model: &str) -> usize {
    // Many local servers (LM Studio/Ollama/etc.) include extra suffixes in model IDs.
    // Prefer substring matching for common models so we don't silently pick the wrong dims.
    if model.contains("text-embedding-3-large") {
        return 3072;
    }
    if model.contains("text-embedding-3-small") || model.contains("text-embedding-ada-002") {
        return 1536;
    }
    if model.contains("nomic-embed-text") {
        return 768;
    }
    if model.contains("mxbai-embed-large") {
        return 1024;
    }
    if model.contains("all-minilm") {
        return 384;
    }
    1536
}

fn default_max_input_chars_for_model(model: &str) -> usize {
    // Conservative defaults: OpenAI-compatible servers vary wildly.
    // For LM Studio + nomic-embed-text-v1.5, this avoids server-side truncation warnings.
    if model.contains("nomic-embed-text") {
        8_000
    } else {
        16_000
    }
}

impl EmbeddingsConfig {
    pub(crate) fn resolve(settings: &Settings) -> Result<Self, ConfigError> {
        let openai_api_key = optional_env("OPENAI_API_KEY")?.map(SecretString::from);
        let embedding_base_url = optional_env("EMBEDDING_BASE_URL")?;
        let llm_base_url = optional_env("LLM_BASE_URL")?;
        let openai_compatible_base_url = embedding_base_url
            .or(llm_base_url)
            .or(settings.openai_compatible_base_url.clone());

        let embedding_api_key = optional_env("EMBEDDING_API_KEY")?;
        let llm_api_key = optional_env("LLM_API_KEY")?;
        let openai_compatible_api_key = embedding_api_key
            .or(llm_api_key)
            .map(SecretString::from);

        let provider = optional_env("EMBEDDING_PROVIDER")?
            .unwrap_or_else(|| settings.embeddings.provider.clone());

        let model =
            optional_env("EMBEDDING_MODEL")?.unwrap_or_else(|| settings.embeddings.model.clone());

        let ollama_base_url = optional_env("OLLAMA_BASE_URL")?
            .or_else(|| settings.ollama_base_url.clone())
            .unwrap_or_else(|| "http://localhost:11434".to_string());

        let dimension =
            parse_optional_env("EMBEDDING_DIMENSION", default_dimension_for_model(&model))?;

        let max_input_chars = parse_optional_env(
            "EMBEDDING_MAX_INPUT_CHARS",
            default_max_input_chars_for_model(&model),
        )?;
        let max_batch_chars = parse_optional_env("EMBEDDING_MAX_BATCH_CHARS", 32_000usize)?;

        let enabled = parse_bool_env("EMBEDDING_ENABLED", settings.embeddings.enabled)?;

        Ok(Self {
            enabled,
            provider,
            openai_api_key,
            openai_compatible_base_url,
            openai_compatible_api_key,
            model,
            ollama_base_url,
            dimension,
            max_input_chars: max_input_chars.max(256),
            max_batch_chars: max_batch_chars.max(256),
        })
    }

    /// Get the OpenAI API key if configured.
    pub fn openai_api_key(&self) -> Option<&str> {
        self.openai_api_key.as_ref().map(|s| s.expose_secret())
    }

    /// Create the appropriate embedding provider based on configuration.
    ///
    /// Returns `None` if embeddings are disabled or the required credentials
    /// are missing.
    pub fn create_provider(&self) -> Option<Arc<dyn EmbeddingProvider>> {
        if !self.enabled {
            tracing::info!("Embeddings disabled (set EMBEDDING_ENABLED=true to enable)");
            return None;
        }

        match self.provider.as_str() {
            "ollama" => {
                tracing::info!(
                    "Embeddings enabled via Ollama (model: {}, url: {}, dim: {})",
                    self.model,
                    self.ollama_base_url,
                    self.dimension,
                );
                Some(Arc::new(
                    crate::workspace::OllamaEmbeddings::new(&self.ollama_base_url)
                        .with_model(&self.model, self.dimension),
                ))
            }
            "openai_compatible" | "openai-compatible" | "compatible" => {
                let Some(ref base_url) = self.openai_compatible_base_url else {
                    tracing::warn!(
                        "Embeddings provider openai_compatible selected but no base URL configured (set EMBEDDING_BASE_URL or LLM_BASE_URL)"
                    );
                    return None;
                };
                let api_key = self
                    .openai_compatible_api_key
                    .as_ref()
                    .map(|s| s.expose_secret().to_string());
                tracing::info!(
                    "Embeddings enabled via OpenAI-compatible (model: {}, base_url: {}, dim: {})",
                    self.model,
                    base_url,
                    self.dimension,
                );
                Some(Arc::new(
                    crate::workspace::OpenAiCompatibleEmbeddings::with_model_and_limits(
                    base_url.clone(),
                    api_key,
                    &self.model,
                    self.dimension,
                    self.max_input_chars,
                    self.max_batch_chars,
                )))
            }
            _ => {
                if let Some(api_key) = self.openai_api_key() {
                    tracing::info!(
                        "Embeddings enabled via OpenAI (model: {}, dim: {})",
                        self.model,
                        self.dimension,
                    );
                    Some(Arc::new(crate::workspace::OpenAiEmbeddings::with_model(
                        api_key,
                        &self.model,
                        self.dimension,
                    )))
                } else {
                    tracing::warn!("Embeddings configured but OPENAI_API_KEY not set");
                    None
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::helpers::ENV_MUTEX;
    use crate::settings::{EmbeddingsSettings, Settings};

    /// Clear all embedding-related env vars.
    fn clear_embedding_env() {
        // SAFETY: Only called under ENV_MUTEX in tests.
        unsafe {
            std::env::remove_var("EMBEDDING_ENABLED");
            std::env::remove_var("EMBEDDING_PROVIDER");
            std::env::remove_var("EMBEDDING_MODEL");
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    #[test]
    fn embeddings_disabled_not_overridden_by_openai_key() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");

        clear_embedding_env();
        // SAFETY: Under ENV_MUTEX, no concurrent env access.
        unsafe {
            std::env::set_var("OPENAI_API_KEY", "sk-test-key-for-issue-129");
        }

        let settings = Settings {
            embeddings: EmbeddingsSettings {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };

        let config = EmbeddingsConfig::resolve(&settings).expect("resolve should succeed");
        assert!(
            !config.enabled,
            "embeddings should remain disabled when settings.embeddings.enabled=false, \
             even when OPENAI_API_KEY is set (issue #129)"
        );

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    #[test]
    fn embeddings_enabled_from_settings() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_embedding_env();

        let settings = Settings {
            embeddings: EmbeddingsSettings {
                enabled: true,
                ..Default::default()
            },
            ..Default::default()
        };

        let config = EmbeddingsConfig::resolve(&settings).expect("resolve should succeed");
        assert!(
            config.enabled,
            "embeddings should be enabled when settings say so"
        );
    }

    #[test]
    fn embeddings_env_override_takes_precedence() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");

        clear_embedding_env();
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("EMBEDDING_ENABLED", "true");
        }

        let settings = Settings {
            embeddings: EmbeddingsSettings {
                enabled: false,
                ..Default::default()
            },
            ..Default::default()
        };

        let config = EmbeddingsConfig::resolve(&settings).expect("resolve should succeed");
        assert!(
            config.enabled,
            "EMBEDDING_ENABLED=true env var should override settings"
        );

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("EMBEDDING_ENABLED");
        }
    }
}
