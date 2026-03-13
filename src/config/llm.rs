use std::path::PathBuf;

use secrecy::SecretString;
use serde_json::Value as JsonValue;

use crate::bootstrap::betterclaw_base_dir;
use crate::config::helpers::{optional_env, parse_optional_env};
use crate::error::ConfigError;
use crate::llm::config::*;
use crate::llm::registry::{ProviderProtocol, ProviderRegistry};
use crate::llm::session::SessionConfig;
use crate::settings::Settings;

impl LlmConfig {
    /// Create a test-friendly config without reading env vars.
    #[cfg(feature = "libsql")]
    pub fn for_testing() -> Self {
        Self {
            backend: "nearai".to_string(),
            session: SessionConfig {
                auth_base_url: "http://localhost:0".to_string(),
                session_path: std::env::temp_dir().join("betterclaw-test-session.json"),
            },
            nearai: NearAiConfig {
                model: "test-model".to_string(),
                cheap_model: None,
                base_url: "http://localhost:0".to_string(),
                api_key: None,
                fallback_model: None,
                max_retries: 0,
                circuit_breaker_threshold: None,
                circuit_breaker_recovery_secs: 30,
                response_cache_enabled: false,
                response_cache_ttl_secs: 3600,
                response_cache_max_entries: 100,
                failover_cooldown_secs: 300,
                failover_cooldown_threshold: 3,
                smart_routing_cascade: false,
            },
            provider: None,
            copilot: None,
            bedrock: None,
            openai_codex: None,
            request_timeout_secs: 120,
        }
    }

    /// Resolve a model name from env var -> settings.selected_model -> hardcoded default.
    fn resolve_model(
        env_var: &str,
        settings: &Settings,
        default: &str,
    ) -> Result<String, ConfigError> {
        Ok(optional_env(env_var)?
            .or_else(|| settings.selected_model.clone())
            .unwrap_or_else(|| default.to_string()))
    }

    pub(crate) fn resolve(settings: &Settings) -> Result<Self, ConfigError> {
        let registry = ProviderRegistry::load();

        // Determine backend: env var > settings > default ("nearai")
        let backend = if let Some(b) = optional_env("LLM_BACKEND")? {
            b
        } else if let Some(ref b) = settings.llm_backend {
            b.clone()
        } else {
            "nearai".to_string()
        };

        // Validate the backend is known
        let backend_lower = backend.to_lowercase();
        let is_nearai =
            backend_lower == "nearai" || backend_lower == "near_ai" || backend_lower == "near";
        let is_openai_codex = matches!(
            backend_lower.as_str(),
            "openai_codex" | "openai-codex" | "codex"
        );
        let is_copilot = matches!(
            backend_lower.as_str(),
            "copilot" | "github_copilot" | "github-copilot"
        );
        let is_bedrock =
            backend_lower == "bedrock" || backend_lower == "aws_bedrock" || backend_lower == "aws";

        if !is_nearai
            && !is_copilot
            && !is_openai_codex
            && !is_bedrock
            && registry.find(&backend_lower).is_none()
        {
            tracing::warn!(
                "Unknown LLM backend '{}'. Will attempt as openai_compatible fallback.",
                backend
            );
        }

        // Session config (used by NearAI provider for OAuth/session-token auth)
        let session = SessionConfig {
            auth_base_url: optional_env("NEARAI_AUTH_URL")?
                .unwrap_or_else(|| "https://private.near.ai".to_string()),
            session_path: optional_env("NEARAI_SESSION_PATH")?
                .map(PathBuf::from)
                .unwrap_or_else(default_session_path),
        };

        // Always resolve NEAR AI config (used for embeddings even when not the primary backend)
        let nearai_api_key = optional_env("NEARAI_API_KEY")?.map(SecretString::from);
        let nearai = NearAiConfig {
            model: Self::resolve_model("NEARAI_MODEL", settings, "zai-org/GLM-latest")?,
            cheap_model: optional_env("NEARAI_CHEAP_MODEL")?,
            base_url: optional_env("NEARAI_BASE_URL")?.unwrap_or_else(|| {
                if nearai_api_key.is_some() {
                    "https://cloud-api.near.ai".to_string()
                } else {
                    "https://private.near.ai".to_string()
                }
            }),
            api_key: nearai_api_key,
            fallback_model: optional_env("NEARAI_FALLBACK_MODEL")?,
            max_retries: parse_optional_env("NEARAI_MAX_RETRIES", 3)?,
            circuit_breaker_threshold: optional_env("CIRCUIT_BREAKER_THRESHOLD")?
                .map(|s| s.parse())
                .transpose()
                .map_err(|e| ConfigError::InvalidValue {
                    key: "CIRCUIT_BREAKER_THRESHOLD".to_string(),
                    message: format!("must be a positive integer: {e}"),
                })?,
            circuit_breaker_recovery_secs: parse_optional_env("CIRCUIT_BREAKER_RECOVERY_SECS", 30)?,
            response_cache_enabled: parse_optional_env("RESPONSE_CACHE_ENABLED", false)?,
            response_cache_ttl_secs: parse_optional_env("RESPONSE_CACHE_TTL_SECS", 3600)?,
            response_cache_max_entries: parse_optional_env("RESPONSE_CACHE_MAX_ENTRIES", 1000)?,
            failover_cooldown_secs: parse_optional_env("LLM_FAILOVER_COOLDOWN_SECS", 300)?,
            failover_cooldown_threshold: parse_optional_env("LLM_FAILOVER_THRESHOLD", 3)?,
            smart_routing_cascade: parse_optional_env("SMART_ROUTING_CASCADE", true)?,
        };

        // Resolve registry provider config (for non-NearAI, non-Bedrock backends)
        let provider = if is_nearai || is_copilot || is_openai_codex || is_bedrock {
            None
        } else {
            Some(Self::resolve_registry_provider(
                &backend_lower,
                &registry,
                settings,
            )?)
        };

        let copilot = if is_copilot {
            let access_token = optional_env("COPILOT_TOKEN")?
                .or(optional_env("GITHUB_COPILOT_TOKEN")?)
                .map(SecretString::from)
                .ok_or_else(|| ConfigError::MissingRequired {
                    key: "COPILOT_TOKEN".to_string(),
                    hint: "Set COPILOT_TOKEN (or GITHUB_COPILOT_TOKEN) when LLM_BACKEND=copilot"
                        .to_string(),
                })?;
            let integration_id =
                optional_env("COPILOT_INTEGRATION_ID")?.ok_or_else(|| ConfigError::MissingRequired {
                    key: "COPILOT_INTEGRATION_ID".to_string(),
                    hint: "Set COPILOT_INTEGRATION_ID when LLM_BACKEND=copilot".to_string(),
                })?;
            let base_url =
                optional_env("COPILOT_API_URL")?.unwrap_or_else(default_copilot_api_url);
            let model = optional_env("COPILOT_MODEL")?
                .or_else(|| settings.selected_model.clone())
                .unwrap_or_else(|| "gpt-4o".to_string());
            let session_id = optional_env("COPILOT_SESSION_ID")?;
            let trace_parent = optional_env("COPILOT_TRACE_PARENT")?;
            let extra_headers = optional_env("COPILOT_EXTRA_HEADERS")?
                .map(|val| parse_extra_headers(&val))
                .transpose()?
                .unwrap_or_default();
            Some(CopilotConfig {
                base_url,
                access_token,
                integration_id,
                model,
                session_id,
                trace_parent,
                extra_headers,
            })
        } else {
            None
        };

        let openai_codex = if is_openai_codex {
            let auth_file = optional_env("OPENAI_CODEX_AUTH_PATH")?
                .unwrap_or_else(default_openai_codex_auth_path);
            let (access_token, account_id) = load_openai_codex_credentials(&auth_file)?;
            let base_url = optional_env("OPENAI_CODEX_BASE_URL")?
                .or_else(|| optional_env("LLM_BASE_URL").ok().flatten())
                .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".to_string());
            let model = optional_env("OPENAI_CODEX_MODEL")?
                .or_else(|| optional_env("LLM_MODEL").ok().flatten())
                .or_else(|| settings.selected_model.clone())
                .unwrap_or_else(|| "gpt-5.3-codex".to_string());
            Some(OpenAiCodexConfig {
                base_url,
                auth_file,
                access_token,
                account_id,
                model,
            })
        } else {
            None
        };

        let bedrock = if is_bedrock {
            let explicit_region =
                optional_env("BEDROCK_REGION")?.or_else(|| settings.bedrock_region.clone());
            if explicit_region.is_none() {
                tracing::info!("BEDROCK_REGION not set, defaulting to us-east-1");
            }
            let region = explicit_region.unwrap_or_else(|| "us-east-1".to_string());
            let model = optional_env("BEDROCK_MODEL")?
                .or_else(|| settings.selected_model.clone())
                .ok_or_else(|| ConfigError::MissingRequired {
                    key: "BEDROCK_MODEL".to_string(),
                    hint: "Set BEDROCK_MODEL when LLM_BACKEND=bedrock".to_string(),
                })?;
            let cross_region = optional_env("BEDROCK_CROSS_REGION")?
                .or_else(|| settings.bedrock_cross_region.clone());
            if let Some(ref cr) = cross_region
                && !matches!(cr.as_str(), "us" | "eu" | "apac" | "global")
            {
                return Err(ConfigError::InvalidValue {
                    key: "BEDROCK_CROSS_REGION".to_string(),
                    message: format!(
                        "'{}' is not valid, expected one of: us, eu, apac, global",
                        cr
                    ),
                });
            }
            let profile = optional_env("AWS_PROFILE")?.or_else(|| settings.bedrock_profile.clone());
            Some(BedrockConfig {
                region,
                model,
                cross_region,
                profile,
            })
        } else {
            None
        };

        let request_timeout_secs = parse_optional_env("LLM_REQUEST_TIMEOUT_SECS", 120)?;

        Ok(Self {
            backend: if is_nearai {
                "nearai".to_string()
            } else if is_copilot {
                "copilot".to_string()
            } else if is_openai_codex {
                "openai_codex".to_string()
            } else if is_bedrock {
                "bedrock".to_string()
            } else if let Some(ref p) = provider {
                p.provider_id.clone()
            } else {
                backend_lower
            },
            session,
            nearai,
            provider,
            copilot,
            bedrock,
            openai_codex,
            request_timeout_secs,
        })
    }

    /// Resolve a `RegistryProviderConfig` from the registry and env vars.
    fn resolve_registry_provider(
        backend: &str,
        registry: &ProviderRegistry,
        settings: &Settings,
    ) -> Result<RegistryProviderConfig, ConfigError> {
        // Look up provider definition. Fall back to openai_compatible if unknown.
        let def = registry
            .find(backend)
            .or_else(|| registry.find("openai_compatible"));

        let (
            canonical_id,
            protocol,
            api_key_env,
            base_url_env,
            model_env,
            default_model,
            default_base_url,
            extra_headers_env,
            api_key_required,
            base_url_required,
            unsupported_params,
        ) = if let Some(def) = def {
            (
                def.id.as_str(),
                def.protocol,
                def.api_key_env.as_deref(),
                def.base_url_env.as_deref(),
                def.model_env.as_str(),
                def.default_model.as_str(),
                def.default_base_url.as_deref(),
                def.extra_headers_env.as_deref(),
                def.api_key_required,
                def.base_url_required,
                def.unsupported_params.clone(),
            )
        } else {
            // Absolute fallback: treat as generic openai_completions
            (
                backend,
                ProviderProtocol::OpenAiCompletions,
                Some("LLM_API_KEY"),
                Some("LLM_BASE_URL"),
                "LLM_MODEL",
                "default",
                None,
                Some("LLM_EXTRA_HEADERS"),
                false,
                true,
                Vec::new(),
            )
        };

        // Resolve API key from env
        let api_key = if let Some(env_var) = api_key_env {
            optional_env(env_var)?.map(SecretString::from)
        } else {
            None
        };

        if api_key_required && api_key.is_none() {
            // Don't hard-fail here. The key might be injected later from the secrets store
            // via inject_llm_keys_from_secrets(). Log a warning instead.
            if let Some(env_var) = api_key_env {
                tracing::debug!(
                    "API key not found in {env_var} for backend '{backend}'. \
                     Will be injected from secrets store if available."
                );
            }
        }

        // Resolve base URL: env var > settings (backward compat) > registry default
        let base_url = if let Some(env_var) = base_url_env {
            optional_env(env_var)?
        } else {
            None
        }
        .or_else(|| {
            // Backward compat: check legacy settings fields
            match backend {
                "ollama" => settings.ollama_base_url.clone(),
                "openai_compatible" | "openrouter" => settings.openai_compatible_base_url.clone(),
                _ => None,
            }
        })
        .or_else(|| default_base_url.map(String::from))
        .unwrap_or_default();

        if base_url_required
            && base_url.is_empty()
            && let Some(env_var) = base_url_env
        {
            return Err(ConfigError::MissingRequired {
                key: env_var.to_string(),
                hint: format!("Set {env_var} when LLM_BACKEND={backend}"),
            });
        }

        // Resolve model
        let model = Self::resolve_model(model_env, settings, default_model)?;

        // Resolve extra headers
        let extra_headers = if let Some(env_var) = extra_headers_env {
            optional_env(env_var)?
                .map(|val| parse_extra_headers(&val))
                .transpose()?
                .unwrap_or_default()
        } else {
            Vec::new()
        };

        // Resolve OAuth token (Anthropic-specific: `claude login` flow).
        // Only check for OAuth token when the provider is actually Anthropic.
        let oauth_token = if canonical_id == "anthropic" {
            optional_env("ANTHROPIC_OAUTH_TOKEN")?.map(SecretString::from)
        } else {
            None
        };
        let api_key = if api_key.is_none() && oauth_token.is_some() {
            // OAuth token present but no API key: use a placeholder so the
            // config block is populated. The provider factory will route to
            // the OAuth provider instead of rig-core's x-api-key client.
            Some(SecretString::from(OAUTH_PLACEHOLDER.to_string()))
        } else {
            api_key
        };

        // Resolve Anthropic prompt cache retention from env (default: Short).
        let cache_retention: CacheRetention = if canonical_id == "anthropic" {
            optional_env("ANTHROPIC_CACHE_RETENTION")?
                .and_then(|val| match val.parse::<CacheRetention>() {
                    Ok(r) => Some(r),
                    Err(e) => {
                        tracing::warn!(
                            "Invalid ANTHROPIC_CACHE_RETENTION: {e}; defaulting to short"
                        );
                        None
                    }
                })
                .unwrap_or_default()
        } else {
            CacheRetention::default()
        };

        Ok(RegistryProviderConfig {
            protocol,
            provider_id: canonical_id.to_string(),
            api_key,
            base_url,
            model,
            extra_headers,
            oauth_token,
            cache_retention,
            unsupported_params,
        })
    }
}

pub fn load_openai_codex_credentials(
    auth_file: &str,
) -> Result<(SecretString, Option<String>), ConfigError> {
    let contents = std::fs::read_to_string(auth_file).map_err(|e| ConfigError::MissingRequired {
        key: "OPENAI_CODEX_AUTH_PATH".to_string(),
        hint: format!("Failed to read Codex auth file at {auth_file}: {e}"),
    })?;

    let parsed: JsonValue =
        serde_json::from_str(&contents).map_err(|e| ConfigError::InvalidValue {
            key: "OPENAI_CODEX_AUTH_PATH".to_string(),
            message: format!("Codex auth file is not valid JSON: {e}"),
        })?;

    let tokens = parsed
        .get("tokens")
        .and_then(|v| v.as_object())
        .ok_or_else(|| ConfigError::InvalidValue {
            key: "OPENAI_CODEX_AUTH_PATH".to_string(),
            message: "Codex auth file is missing the 'tokens' object".to_string(),
        })?;

    let access_token = tokens
        .get("access_token")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ConfigError::MissingRequired {
            key: "OPENAI_CODEX_AUTH_PATH".to_string(),
            hint: format!("Codex auth file at {auth_file} is missing tokens.access_token"),
        })?;

    let account_id = tokens
        .get("account_id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(ToOwned::to_owned);

    Ok((SecretString::from(access_token.to_string()), account_id))
}

pub fn build_copilot_headers(config: &CopilotConfig) -> Vec<(String, String)> {
    let mut headers = vec![(
        "Copilot-Integration-Id".to_string(),
        config.integration_id.clone(),
    )];

    if let Some(ref session_id) = config.session_id {
        headers.push(("X-Copilot-Session-Id".to_string(), session_id.clone()));
    }

    if let Some(ref trace_parent) = config.trace_parent {
        headers.push(("X-Copilot-Traceparent".to_string(), trace_parent.clone()));
    }

    headers.extend(config.extra_headers.clone());
    headers
}

/// Parse `LLM_EXTRA_HEADERS` value into a list of (key, value) pairs.
///
/// Format: `Key1:Value1,Key2:Value2` (colon-separated, not `=`, because
/// header values often contain `=`).
pub fn parse_extra_headers(val: &str) -> Result<Vec<(String, String)>, ConfigError> {
    if val.trim().is_empty() {
        return Ok(Vec::new());
    }

    let mut headers = Vec::new();
    for pair in val.split(',') {
        let pair = pair.trim();
        if pair.is_empty() {
            continue;
        }
        let Some((key, value)) = pair.split_once(':') else {
            return Err(ConfigError::InvalidValue {
                key: "LLM_EXTRA_HEADERS".to_string(),
                message: format!("malformed header entry '{}', expected Key:Value", pair),
            });
        };
        let key = key.trim();
        if key.is_empty() {
            return Err(ConfigError::InvalidValue {
                key: "LLM_EXTRA_HEADERS".to_string(),
                message: format!("empty header name in entry '{}'", pair),
            });
        }
        headers.push((key.to_string(), value.trim().to_string()));
    }
    Ok(headers)
}

/// Get the default session file path (~/.betterclaw/session.json).
pub fn default_session_path() -> PathBuf {
    betterclaw_base_dir().join("session.json")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::helpers::ENV_MUTEX;
    use crate::settings::Settings;
    use crate::testing::credentials::*;

    /// Clear all openai-compatible-related env vars.
    fn clear_openai_compatible_env() {
        // SAFETY: Only called under ENV_MUTEX in tests.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("LLM_BASE_URL");
            std::env::remove_var("LLM_MODEL");
        }
    }

    fn clear_openai_codex_env() {
        // SAFETY: Only called under ENV_MUTEX in tests.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("LLM_BASE_URL");
            std::env::remove_var("LLM_MODEL");
            std::env::remove_var("OPENAI_CODEX_AUTH_PATH");
            std::env::remove_var("OPENAI_CODEX_BASE_URL");
            std::env::remove_var("OPENAI_CODEX_MODEL");
        }
    }

    fn clear_copilot_env() {
        // SAFETY: Only called under ENV_MUTEX in tests.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("COPILOT_TOKEN");
            std::env::remove_var("GITHUB_COPILOT_TOKEN");
            std::env::remove_var("COPILOT_INTEGRATION_ID");
            std::env::remove_var("COPILOT_API_URL");
            std::env::remove_var("COPILOT_MODEL");
            std::env::remove_var("COPILOT_SESSION_ID");
            std::env::remove_var("COPILOT_TRACE_PARENT");
            std::env::remove_var("COPILOT_EXTRA_HEADERS");
            std::env::remove_var("LLM_BASE_URL");
        }
    }

    #[test]
    fn openai_compatible_uses_selected_model_when_llm_model_unset() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_compatible_env();

        let settings = Settings {
            llm_backend: Some("openai_compatible".to_string()),
            openai_compatible_base_url: Some("https://openrouter.ai/api/v1".to_string()),
            selected_model: Some("openai/gpt-5.1-codex".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("provider config should be present");

        assert_eq!(provider.model, "openai/gpt-5.1-codex");
    }

    #[test]
    fn copilot_uses_default_base_url_without_llm_base_url() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_copilot_env();
        unsafe {
            std::env::set_var("LLM_BACKEND", "copilot");
            std::env::set_var("COPILOT_TOKEN", "ghu_copilot_test_token");
            std::env::set_var("COPILOT_INTEGRATION_ID", "vscode-chat");
        }

        let cfg = LlmConfig::resolve(&Settings::default()).expect("resolve should succeed");
        let copilot = cfg.copilot.expect("copilot config should be present");

        assert_eq!(cfg.backend, "copilot");
        assert_eq!(copilot.base_url, "https://api.githubcopilot.com");
    }

    #[test]
    fn openai_compatible_llm_model_env_overrides_selected_model() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_compatible_env();
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("LLM_MODEL", "openai/gpt-5-codex");
        }

        let settings = Settings {
            llm_backend: Some("openai_compatible".to_string()),
            openai_compatible_base_url: Some("https://openrouter.ai/api/v1".to_string()),
            selected_model: Some("openai/gpt-5.1-codex".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("provider config should be present");

        assert_eq!(provider.model, "openai/gpt-5-codex");

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_MODEL");
        }
    }

    #[test]
    fn test_extra_headers_parsed() {
        let result = parse_extra_headers("HTTP-Referer:https://myapp.com,X-Title:MyApp").unwrap();
        assert_eq!(
            result,
            vec![
                ("HTTP-Referer".to_string(), "https://myapp.com".to_string()),
                ("X-Title".to_string(), "MyApp".to_string()),
            ]
        );
    }

    #[test]
    fn test_extra_headers_empty_string() {
        let result = parse_extra_headers("").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_extra_headers_whitespace_only() {
        let result = parse_extra_headers("  ").unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_extra_headers_malformed() {
        let result = parse_extra_headers("NoColonHere");
        assert!(result.is_err());
    }

    #[test]
    fn test_extra_headers_empty_key() {
        let result = parse_extra_headers(":value");
        assert!(result.is_err());
    }

    #[test]
    fn test_extra_headers_value_with_colons() {
        let result = parse_extra_headers("Authorization:Bearer abc:def").unwrap();
        assert_eq!(
            result,
            vec![("Authorization".to_string(), "Bearer abc:def".to_string())]
        );
    }

    #[test]
    fn test_extra_headers_trailing_comma() {
        let result = parse_extra_headers("X-Title:MyApp,").unwrap();
        assert_eq!(result, vec![("X-Title".to_string(), "MyApp".to_string())]);
    }

    #[test]
    fn test_extra_headers_with_spaces() {
        let result =
            parse_extra_headers(" HTTP-Referer : https://myapp.com , X-Title : MyApp ").unwrap();
        assert_eq!(
            result,
            vec![
                ("HTTP-Referer".to_string(), "https://myapp.com".to_string()),
                ("X-Title".to_string(), "MyApp".to_string()),
            ]
        );
    }

    /// Clear all ollama-related env vars.
    fn clear_ollama_env() {
        // SAFETY: Only called under ENV_MUTEX in tests.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("OLLAMA_BASE_URL");
            std::env::remove_var("OLLAMA_MODEL");
        }
    }

    #[test]
    fn ollama_uses_selected_model_when_ollama_model_unset() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_ollama_env();

        let settings = Settings {
            llm_backend: Some("ollama".to_string()),
            selected_model: Some("llama3.2".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("provider config should be present");

        assert_eq!(provider.model, "llama3.2");
    }

    #[test]
    fn ollama_model_env_overrides_selected_model() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_ollama_env();
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("OLLAMA_MODEL", "mistral:latest");
        }

        let settings = Settings {
            llm_backend: Some("ollama".to_string()),
            selected_model: Some("llama3.2".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("provider config should be present");

        assert_eq!(provider.model, "mistral:latest");

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("OLLAMA_MODEL");
        }
    }

    #[test]
    fn openai_compatible_preserves_dotted_model_name() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_compatible_env();

        let settings = Settings {
            llm_backend: Some("openai_compatible".to_string()),
            openai_compatible_base_url: Some("http://localhost:11434/v1".to_string()),
            selected_model: Some("llama3.2".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("provider config should be present");

        assert_eq!(
            provider.model, "llama3.2",
            "model name with dot must not be truncated"
        );
    }

    #[test]
    fn registry_provider_resolves_groq() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("GROQ_API_KEY");
            std::env::remove_var("GROQ_MODEL");
        }

        let settings = Settings {
            llm_backend: Some("groq".to_string()),
            selected_model: Some("llama-3.3-70b-versatile".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        assert_eq!(cfg.backend, "groq");
        let provider = cfg.provider.expect("provider config should be present");
        assert_eq!(provider.provider_id, "groq");
        assert_eq!(provider.model, "llama-3.3-70b-versatile");
        assert_eq!(provider.base_url, "https://api.groq.com/openai/v1");
        assert_eq!(provider.protocol, ProviderProtocol::OpenAiCompletions);
    }

    #[test]
    fn registry_provider_resolves_tinfoil() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("TINFOIL_API_KEY");
            std::env::remove_var("TINFOIL_MODEL");
        }

        let settings = Settings {
            llm_backend: Some("tinfoil".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        assert_eq!(cfg.backend, "tinfoil");
        let provider = cfg.provider.expect("provider config should be present");
        assert_eq!(provider.base_url, "https://inference.tinfoil.sh/v1");
        assert_eq!(provider.model, "kimi-k2-5");
        assert!(
            provider
                .unsupported_params
                .contains(&"temperature".to_string()),
            "tinfoil should propagate unsupported_params from registry"
        );
    }

    #[test]
    fn nearai_backend_has_no_registry_provider() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
        }

        let settings = Settings::default();
        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        assert_eq!(cfg.backend, "nearai");
        assert!(cfg.provider.is_none());
    }

    #[test]
    fn backend_alias_normalized_to_canonical_id() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_compatible_env();
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("LLM_BACKEND", "open_ai");
            std::env::set_var("OPENAI_API_KEY", TEST_API_KEY);
        }

        let settings = Settings::default();
        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        assert_eq!(
            cfg.backend, "openai",
            "alias 'open_ai' should be normalized to canonical 'openai'"
        );
        let provider = cfg.provider.expect("should have provider config");
        assert_eq!(provider.provider_id, "openai");

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("OPENAI_API_KEY");
        }
    }

    #[test]
    fn unknown_backend_falls_back_to_openai_compatible() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_compatible_env();
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("LLM_BACKEND", "some_custom_provider");
            std::env::set_var("LLM_BASE_URL", "http://localhost:8080/v1");
        }

        let settings = Settings::default();
        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        assert_eq!(cfg.backend, "openai_compatible");
        let provider = cfg.provider.expect("should have provider config");
        assert_eq!(provider.provider_id, "openai_compatible");
        assert_eq!(provider.base_url, "http://localhost:8080/v1");

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("LLM_BASE_URL");
        }
    }

    #[test]
    fn nearai_aliases_all_resolve_to_nearai() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");

        for alias in &["nearai", "near_ai", "near"] {
            // SAFETY: Under ENV_MUTEX.
            unsafe {
                std::env::set_var("LLM_BACKEND", alias);
            }
            let settings = Settings::default();
            let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
            assert_eq!(
                cfg.backend, "nearai",
                "alias '{alias}' should resolve to 'nearai'"
            );
            assert!(
                cfg.provider.is_none(),
                "nearai should not have a registry provider"
            );
        }

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
        }
    }

    #[test]
    fn base_url_resolution_priority() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_compatible_env();

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("LLM_BACKEND", "openai_compatible");
            std::env::set_var("LLM_BASE_URL", "http://env-url/v1");
        }

        let settings = Settings {
            llm_backend: Some("openai_compatible".to_string()),
            openai_compatible_base_url: Some("http://settings-url/v1".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("should have provider config");
        assert_eq!(
            provider.base_url, "http://env-url/v1",
            "env var should take priority over settings"
        );

        // Now without env var, settings should win over registry default
        unsafe {
            std::env::remove_var("LLM_BASE_URL");
        }

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("should have provider config");
        assert_eq!(
            provider.base_url, "http://settings-url/v1",
            "settings should take priority over registry default"
        );

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
        }
    }

    #[test]
    fn openai_codex_backend_resolves_primary_provider_config() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_codex_env();

        let auth_path = std::env::temp_dir().join(format!(
            "betterclaw-codex-auth-{}.json",
            std::process::id()
        ));
        std::fs::write(
            &auth_path,
            r#"{"tokens":{"access_token":"test-access-token","account_id":"acct_123"}}"#,
        )
        .expect("write temp auth file");

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("LLM_BACKEND", "openai_codex");
            std::env::set_var("LLM_BASE_URL", "https://openrouter.ai/api/v1");
            std::env::set_var("LLM_MODEL", "moonshotai/kimi-k2.5");
            std::env::set_var("OPENAI_CODEX_AUTH_PATH", &auth_path);
        }

        let settings = Settings::default();
        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        assert_eq!(cfg.backend, "openai_codex");
        assert!(cfg.provider.is_none(), "codex should not fall back to registry provider");
        let codex = cfg.openai_codex.expect("codex config should be present");
        assert_eq!(codex.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(codex.model, "moonshotai/kimi-k2.5");
        assert_eq!(codex.account_id.as_deref(), Some("acct_123"));

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("LLM_BASE_URL");
            std::env::remove_var("LLM_MODEL");
            std::env::remove_var("OPENAI_CODEX_AUTH_PATH");
        }
        let _ = std::fs::remove_file(&auth_path);
    }

    // ── OAuth resolution tests ──────────────────────────────────────

    /// Clear all Anthropic-related env vars.
    fn clear_anthropic_env() {
        // SAFETY: Only called under ENV_MUTEX in tests.
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("ANTHROPIC_API_KEY");
            std::env::remove_var("ANTHROPIC_OAUTH_TOKEN");
            std::env::remove_var("ANTHROPIC_MODEL");
            std::env::remove_var("ANTHROPIC_BASE_URL");
        }
    }

    #[test]
    fn anthropic_oauth_token_sets_placeholder_api_key() {
        use secrecy::ExposeSecret;

        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_anthropic_env();
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("ANTHROPIC_OAUTH_TOKEN", TEST_ANTHROPIC_OAUTH_TOKEN);
        }

        let settings = Settings {
            llm_backend: Some("anthropic".to_string()),
            ..Default::default()
        };
        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("provider config should be present");

        assert_eq!(
            provider
                .api_key
                .as_ref()
                .map(|k| k.expose_secret().to_string()),
            Some(OAUTH_PLACEHOLDER.to_string()),
            "api_key should be the OAuth placeholder when only OAuth token is set"
        );
        assert!(
            provider.oauth_token.is_some(),
            "oauth_token should be populated"
        );
        assert_eq!(
            provider.oauth_token.as_ref().unwrap().expose_secret(),
            TEST_ANTHROPIC_OAUTH_TOKEN
        );

        clear_anthropic_env();
    }

    #[test]
    fn anthropic_api_key_takes_priority_over_oauth() {
        use secrecy::ExposeSecret;

        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_anthropic_env();
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("ANTHROPIC_API_KEY", TEST_ANTHROPIC_API_KEY);
            std::env::set_var("ANTHROPIC_OAUTH_TOKEN", TEST_ANTHROPIC_OAUTH_TOKEN);
        }

        let settings = Settings {
            llm_backend: Some("anthropic".to_string()),
            ..Default::default()
        };
        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("provider config should be present");

        assert_eq!(
            provider
                .api_key
                .as_ref()
                .map(|k| k.expose_secret().to_string()),
            Some(TEST_ANTHROPIC_API_KEY.to_string()),
            "real API key should take priority over OAuth placeholder"
        );
        assert!(
            provider.oauth_token.is_some(),
            "oauth_token should still be populated"
        );

        clear_anthropic_env();
    }

    #[test]
    fn non_anthropic_provider_has_no_oauth_token() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_anthropic_env();
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("ANTHROPIC_OAUTH_TOKEN", TEST_ANTHROPIC_OAUTH_TOKEN);
        }

        let settings = Settings {
            llm_backend: Some("openai".to_string()),
            ..Default::default()
        };
        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let provider = cfg.provider.expect("provider config should be present");

        assert!(
            provider.oauth_token.is_none(),
            "non-Anthropic providers should not pick up ANTHROPIC_OAUTH_TOKEN"
        );

        clear_anthropic_env();
    }

    // ── Cache retention tests ───────────────────────────────────────

    #[test]
    fn cache_retention_from_str_primary_values() {
        assert_eq!(
            "none".parse::<CacheRetention>().unwrap(),
            CacheRetention::None
        );
        assert_eq!(
            "short".parse::<CacheRetention>().unwrap(),
            CacheRetention::Short
        );
        assert_eq!(
            "long".parse::<CacheRetention>().unwrap(),
            CacheRetention::Long
        );
    }

    #[test]
    fn cache_retention_from_str_aliases() {
        assert_eq!(
            "off".parse::<CacheRetention>().unwrap(),
            CacheRetention::None
        );
        assert_eq!(
            "disabled".parse::<CacheRetention>().unwrap(),
            CacheRetention::None
        );
        assert_eq!(
            "5m".parse::<CacheRetention>().unwrap(),
            CacheRetention::Short
        );
        assert_eq!(
            "ephemeral".parse::<CacheRetention>().unwrap(),
            CacheRetention::Short
        );
        assert_eq!(
            "1h".parse::<CacheRetention>().unwrap(),
            CacheRetention::Long
        );
    }

    #[test]
    fn cache_retention_from_str_case_insensitive() {
        assert_eq!(
            "NONE".parse::<CacheRetention>().unwrap(),
            CacheRetention::None
        );
        assert_eq!(
            "Short".parse::<CacheRetention>().unwrap(),
            CacheRetention::Short
        );
        assert_eq!(
            "LONG".parse::<CacheRetention>().unwrap(),
            CacheRetention::Long
        );
        assert_eq!(
            "Ephemeral".parse::<CacheRetention>().unwrap(),
            CacheRetention::Short
        );
    }

    #[test]
    fn cache_retention_from_str_invalid() {
        let err = "bogus".parse::<CacheRetention>().unwrap_err();
        assert!(
            err.contains("bogus"),
            "error should mention the invalid value"
        );
    }

    #[test]
    fn cache_retention_display_round_trip() {
        for variant in [
            CacheRetention::None,
            CacheRetention::Short,
            CacheRetention::Long,
        ] {
            let s = variant.to_string();
            let parsed: CacheRetention = s.parse().unwrap();
            assert_eq!(parsed, variant, "round-trip failed for {s}");
        }
    }

    #[test]
    fn test_request_timeout_defaults_to_120() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_REQUEST_TIMEOUT_SECS");
        }
        let config = LlmConfig::resolve(&Settings::default()).expect("resolve");
        assert_eq!(config.request_timeout_secs, 120);
    }

    #[test]
    fn test_request_timeout_configurable() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::set_var("LLM_REQUEST_TIMEOUT_SECS", "300");
        }
        let config = LlmConfig::resolve(&Settings::default()).expect("resolve");
        assert_eq!(config.request_timeout_secs, 300);
        // SAFETY: Cleanup
        unsafe {
            std::env::remove_var("LLM_REQUEST_TIMEOUT_SECS");
        }
    }
}
