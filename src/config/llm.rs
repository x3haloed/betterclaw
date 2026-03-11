use secrecy::SecretString;
use serde::Deserialize;

use crate::config::helpers::{optional_env, parse_optional_env};
use crate::error::ConfigError;
use crate::settings::Settings;

/// Which LLM backend to use.
///
/// Users can override with `LLM_BACKEND` env var to use their preferred provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum LlmBackend {
    /// Direct OpenAI API
    OpenAi,
    /// Direct Anthropic API
    Anthropic,
    /// Local Ollama instance
    #[default]
    Ollama,
    /// Any OpenAI-compatible endpoint (e.g. vLLM, LiteLLM, Together)
    OpenAiCompatible,
    /// GitHub Copilot Chat Completions endpoint
    Copilot,
    /// OpenAI Codex mode authenticated via ~/.codex/auth.json
    OpenAiCodex,
    /// Tinfoil private inference
    Tinfoil,
}

impl std::str::FromStr for LlmBackend {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_lowercase().as_str() {
            "openai" | "open_ai" => Ok(Self::OpenAi),
            "anthropic" | "claude" => Ok(Self::Anthropic),
            "ollama" => Ok(Self::Ollama),
            "openai_compatible" | "openai-compatible" | "compatible" => Ok(Self::OpenAiCompatible),
            "copilot" | "github_copilot" | "github-copilot" => Ok(Self::Copilot),
            "openai_codex" | "openai-codex" | "codex" => Ok(Self::OpenAiCodex),
            "tinfoil" => Ok(Self::Tinfoil),
            _ => Err(format!(
                "invalid LLM backend '{}', expected one of: openai, anthropic, ollama, openai_compatible, copilot, openai_codex, tinfoil",
                s
            )),
        }
    }
}

impl std::fmt::Display for LlmBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::OpenAi => write!(f, "openai"),
            Self::Anthropic => write!(f, "anthropic"),
            Self::Ollama => write!(f, "ollama"),
            Self::OpenAiCompatible => write!(f, "openai_compatible"),
            Self::Copilot => write!(f, "copilot"),
            Self::OpenAiCodex => write!(f, "openai_codex"),
            Self::Tinfoil => write!(f, "tinfoil"),
        }
    }
}

impl LlmBackend {
    /// The environment variable that configures the model name for this backend.
    ///
    /// Used by both `LlmConfig::resolve()` (reads the var) and the setup wizard
    /// (writes the var to `.env`). Centralised here so the two stay in sync.
    pub fn model_env_var(&self) -> &'static str {
        match self {
            Self::OpenAi => "OPENAI_MODEL",
            Self::Anthropic => "ANTHROPIC_MODEL",
            Self::Ollama => "OLLAMA_MODEL",
            Self::OpenAiCompatible => "LLM_MODEL",
            Self::Copilot => "COPILOT_MODEL",
            Self::OpenAiCodex => "OPENAI_CODEX_MODEL",
            Self::Tinfoil => "TINFOIL_MODEL",
        }
    }
}

/// Configuration for direct OpenAI API access.
#[derive(Debug, Clone)]
pub struct OpenAiDirectConfig {
    pub api_key: SecretString,
    pub model: String,
    /// Optional base URL override (e.g. for proxies like VibeProxy).
    pub base_url: Option<String>,
}

/// Configuration for direct Anthropic API access.
#[derive(Debug, Clone)]
pub struct AnthropicDirectConfig {
    pub api_key: SecretString,
    pub model: String,
    /// Optional base URL override (e.g. for proxies like VibeProxy).
    pub base_url: Option<String>,
}

/// Configuration for local Ollama.
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    pub base_url: String,
    pub model: String,
}

/// Configuration for any OpenAI-compatible endpoint.
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    pub base_url: String,
    pub api_key: Option<SecretString>,
    pub model: String,
    /// Extra HTTP headers injected into every LLM request.
    /// Parsed from `LLM_EXTRA_HEADERS` env var (format: `Key:Value,Key2:Value2`).
    pub extra_headers: Vec<(String, String)>,
}

/// Configuration for GitHub Copilot's chat completions endpoint.
#[derive(Debug, Clone)]
pub struct CopilotConfig {
    pub base_url: String,
    pub access_token: SecretString,
    pub integration_id: String,
    pub model: String,
    pub session_id: Option<String>,
    pub trace_parent: Option<String>,
    pub extra_headers: Vec<(String, String)>,
}

/// Configuration for OpenAI Codex mode authenticated via ~/.codex/auth.json.
#[derive(Debug, Clone)]
pub struct OpenAiCodexConfig {
    pub base_url: String,
    pub auth_file: String,
    pub access_token: SecretString,
    pub account_id: Option<String>,
    pub model: String,
}

/// Configuration for Tinfoil private inference.
#[derive(Debug, Clone)]
pub struct TinfoilConfig {
    pub api_key: SecretString,
    pub model: String,
}

/// LLM provider configuration.
#[derive(Debug, Clone)]
pub struct LlmConfig {
    /// Which backend to use (default: Ollama)
    pub backend: LlmBackend,
    /// Cross-provider tuning knobs (retry, failover, etc.).
    pub tuning: LlmTuningConfig,
    /// Direct OpenAI config (populated when backend=openai)
    pub openai: Option<OpenAiDirectConfig>,
    /// Direct Anthropic config (populated when backend=anthropic)
    pub anthropic: Option<AnthropicDirectConfig>,
    /// Ollama config (populated when backend=ollama)
    pub ollama: Option<OllamaConfig>,
    /// OpenAI-compatible config (populated when backend=openai_compatible)
    pub openai_compatible: Option<OpenAiCompatibleConfig>,
    /// GitHub Copilot config (populated when backend=copilot)
    pub copilot: Option<CopilotConfig>,
    /// OpenAI Codex config (populated when backend=openai_codex)
    pub openai_codex: Option<OpenAiCodexConfig>,
    /// Tinfoil config (populated when backend=tinfoil)
    pub tinfoil: Option<TinfoilConfig>,
}

#[derive(Debug, Deserialize)]
struct CodexAuthFile {
    #[serde(default)]
    tokens: CodexTokens,
}

#[derive(Debug, Default, Deserialize)]
struct CodexTokens {
    access_token: Option<String>,
    account_id: Option<String>,
}

pub(crate) fn default_openai_codex_auth_path() -> String {
    if let Some(home) = dirs::home_dir() {
        return home.join(".codex").join("auth.json").display().to_string();
    }
    ".codex/auth.json".to_string()
}

pub(crate) fn default_copilot_api_url() -> String {
    "https://api.githubcopilot.com".to_string()
}

pub(crate) fn build_copilot_headers(config: &CopilotConfig) -> Vec<(String, String)> {
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

pub(crate) fn load_openai_codex_credentials(
    auth_file: &str,
) -> Result<(SecretString, Option<String>), ConfigError> {
    let content = std::fs::read_to_string(auth_file).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => ConfigError::MissingRequired {
            key: "OPENAI_CODEX_AUTH_PATH".to_string(),
            hint: format!(
                "OpenAI Codex mode expects a Codex auth file at {} (or set OPENAI_CODEX_AUTH_PATH).",
                auth_file
            ),
        },
        _ => e.into(),
    })?;

    let auth: CodexAuthFile = serde_json::from_str(&content).map_err(|e| {
        ConfigError::ParseError(format!(
            "Failed to parse Codex auth file {}: {}",
            auth_file, e
        ))
    })?;

    let access_token = auth
        .tokens
        .access_token
        .filter(|s| !s.trim().is_empty())
        .ok_or_else(|| ConfigError::MissingRequired {
            key: "OPENAI_CODEX_AUTH_PATH".to_string(),
            hint: format!(
                "Codex auth file {} does not contain tokens.access_token.",
                auth_file
            ),
        })?;

    Ok((SecretString::from(access_token), auth.tokens.account_id))
}

#[derive(Debug, Clone)]
pub struct LlmTuningConfig {
    /// Cheap/fast model for lightweight tasks (heartbeat, routing, evaluation).
    ///
    /// When set, enables cheap/primary smart routing in the provider chain.
    pub cheap_model: Option<String>,
    /// Optional fallback model for failover (default: None).
    /// Maximum number of retries for transient errors (default: 3).
    /// With the default of 3, the provider makes up to 4 total attempts
    /// (1 initial + 3 retries) before giving up.
    pub max_retries: u32,
    /// Optional fallback model for failover (default: None).
    pub fallback_model: Option<String>,
    /// Consecutive transient failures before the circuit breaker opens.
    /// None = disabled (default). E.g. 5 means after 5 consecutive failures
    /// all requests are rejected until recovery timeout elapses.
    pub circuit_breaker_threshold: Option<u32>,
    /// How long (seconds) the circuit stays open before allowing a probe (default: 30).
    pub circuit_breaker_recovery_secs: u64,
    /// Enable in-memory response caching for `complete()` calls.
    /// Saves tokens on repeated prompts within a session. Default: false.
    pub response_cache_enabled: bool,
    /// TTL in seconds for cached responses (default: 3600 = 1 hour).
    pub response_cache_ttl_secs: u64,
    /// Max cached responses before LRU eviction (default: 1000).
    pub response_cache_max_entries: usize,
    /// Cooldown duration in seconds for the failover provider (default: 300).
    /// When a provider accumulates enough consecutive failures it is skipped
    /// for this many seconds.
    pub failover_cooldown_secs: u64,
    /// Number of consecutive retryable failures before a provider enters
    /// cooldown (default: 3).
    pub failover_cooldown_threshold: u32,
    /// Enable cascade mode for smart routing: when a moderate-complexity task
    /// gets an uncertain response from the cheap model, re-send to primary.
    /// Default: true.
    pub smart_routing_cascade: bool,
}

impl LlmConfig {
    /// Resolve a model name from env var → settings.selected_model → hardcoded default.
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
        // Determine backend: env var > settings > default (Ollama)
        let backend: LlmBackend = if let Some(b) = optional_env("LLM_BACKEND")? {
            b.parse().map_err(|e| ConfigError::InvalidValue {
                key: "LLM_BACKEND".to_string(),
                message: e,
            })?
        } else if let Some(ref b) = settings.llm_backend {
            match b.parse() {
                Ok(backend) => backend,
                Err(e) => {
                    tracing::warn!(
                        "Invalid llm_backend '{}' in settings: {}. Using default Ollama.",
                        b,
                        e
                    );
                    LlmBackend::Ollama
                }
            }
        } else {
            LlmBackend::Ollama
        };

        let tuning = LlmTuningConfig {
            cheap_model: optional_env("LLM_CHEAP_MODEL")?,
            fallback_model: optional_env("LLM_FALLBACK_MODEL")?,
            max_retries: parse_optional_env("LLM_MAX_RETRIES", 3)?,
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

        // Resolve provider-specific configs based on backend
        let openai = if backend == LlmBackend::OpenAi {
            let api_key = optional_env("OPENAI_API_KEY")?
                .map(SecretString::from)
                .ok_or_else(|| ConfigError::MissingRequired {
                    key: "OPENAI_API_KEY".to_string(),
                    hint: "Set OPENAI_API_KEY when LLM_BACKEND=openai".to_string(),
                })?;
            let model = Self::resolve_model("OPENAI_MODEL", settings, "gpt-4o")?;
            let base_url = optional_env("OPENAI_BASE_URL")?;
            Some(OpenAiDirectConfig {
                api_key,
                model,
                base_url,
            })
        } else {
            None
        };

        let anthropic = if backend == LlmBackend::Anthropic {
            let api_key = optional_env("ANTHROPIC_API_KEY")?
                .map(SecretString::from)
                .ok_or_else(|| ConfigError::MissingRequired {
                    key: "ANTHROPIC_API_KEY".to_string(),
                    hint: "Set ANTHROPIC_API_KEY when LLM_BACKEND=anthropic".to_string(),
                })?;
            let model =
                Self::resolve_model("ANTHROPIC_MODEL", settings, "claude-sonnet-4-20250514")?;
            let base_url = optional_env("ANTHROPIC_BASE_URL")?;
            Some(AnthropicDirectConfig {
                api_key,
                model,
                base_url,
            })
        } else {
            None
        };

        let ollama = if backend == LlmBackend::Ollama {
            let base_url = optional_env("OLLAMA_BASE_URL")?
                .or_else(|| settings.ollama_base_url.clone())
                .unwrap_or_else(|| "http://localhost:11434".to_string());
            let model = Self::resolve_model("OLLAMA_MODEL", settings, "llama3")?;
            Some(OllamaConfig { base_url, model })
        } else {
            None
        };

        let openai_compatible = if backend == LlmBackend::OpenAiCompatible {
            let base_url = optional_env("LLM_BASE_URL")?
                .or_else(|| settings.openai_compatible_base_url.clone())
                .ok_or_else(|| ConfigError::MissingRequired {
                    key: "LLM_BASE_URL".to_string(),
                    hint: "Set LLM_BASE_URL when LLM_BACKEND=openai_compatible".to_string(),
                })?;
            let api_key = optional_env("LLM_API_KEY")?.map(SecretString::from);
            let model = Self::resolve_model("LLM_MODEL", settings, "default")?;
            let extra_headers = optional_env("LLM_EXTRA_HEADERS")?
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

        let copilot = if backend == LlmBackend::Copilot {
            let access_token = optional_env("COPILOT_TOKEN")?
                .or(optional_env("GITHUB_COPILOT_TOKEN")?)
                .map(SecretString::from)
                .ok_or_else(|| ConfigError::MissingRequired {
                    key: "COPILOT_TOKEN".to_string(),
                    hint: "Set COPILOT_TOKEN (or GITHUB_COPILOT_TOKEN) when LLM_BACKEND=copilot"
                        .to_string(),
                })?;
            let integration_id = optional_env("COPILOT_INTEGRATION_ID")?.ok_or_else(|| {
                ConfigError::MissingRequired {
                    key: "COPILOT_INTEGRATION_ID".to_string(),
                    hint: "Set COPILOT_INTEGRATION_ID when LLM_BACKEND=copilot".to_string(),
                }
            })?;
            let base_url = optional_env("COPILOT_API_URL")?.unwrap_or_else(default_copilot_api_url);
            let model = Self::resolve_model("COPILOT_MODEL", settings, "gpt-4o")?;
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

        let openai_codex = if backend == LlmBackend::OpenAiCodex {
            let auth_file = optional_env("OPENAI_CODEX_AUTH_PATH")?
                .unwrap_or_else(default_openai_codex_auth_path);
            let (access_token, account_id) = load_openai_codex_credentials(&auth_file)?;
            let base_url = optional_env("OPENAI_CODEX_BASE_URL")?
                .unwrap_or_else(|| "https://chatgpt.com/backend-api/codex".to_string());
            let model = Self::resolve_model("OPENAI_CODEX_MODEL", settings, "gpt-5.3-codex")?;
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

        let tinfoil = if backend == LlmBackend::Tinfoil {
            let api_key = optional_env("TINFOIL_API_KEY")?
                .map(SecretString::from)
                .ok_or_else(|| ConfigError::MissingRequired {
                    key: "TINFOIL_API_KEY".to_string(),
                    hint: "Set TINFOIL_API_KEY when LLM_BACKEND=tinfoil".to_string(),
                })?;
            let model = Self::resolve_model("TINFOIL_MODEL", settings, "kimi-k2-5")?;
            Some(TinfoilConfig { api_key, model })
        } else {
            None
        };

        Ok(Self {
            backend,
            tuning,
            openai,
            anthropic,
            ollama,
            openai_compatible,
            copilot,
            openai_codex,
            tinfoil,
        })
    }
}

/// Parse `LLM_EXTRA_HEADERS` value into a list of (key, value) pairs.
///
/// Format: `Key1:Value1,Key2:Value2` — colon-separated key:value, comma-separated pairs.
/// Colon is used as the separator (not `=`) because header values often contain `=`
/// (e.g., base64 tokens).
pub(crate) fn parse_extra_headers(val: &str) -> Result<Vec<(String, String)>, ConfigError> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::helpers::ENV_MUTEX;
    use crate::settings::Settings;

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
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("OPENAI_CODEX_AUTH_PATH");
            std::env::remove_var("OPENAI_CODEX_MODEL");
            std::env::remove_var("OPENAI_CODEX_BASE_URL");
        }
    }

    fn clear_copilot_env() {
        unsafe {
            std::env::remove_var("LLM_BACKEND");
            std::env::remove_var("COPILOT_TOKEN");
            std::env::remove_var("GITHUB_COPILOT_TOKEN");
            std::env::remove_var("COPILOT_INTEGRATION_ID");
            std::env::remove_var("COPILOT_MODEL");
            std::env::remove_var("COPILOT_API_URL");
            std::env::remove_var("COPILOT_SESSION_ID");
            std::env::remove_var("COPILOT_TRACE_PARENT");
            std::env::remove_var("COPILOT_EXTRA_HEADERS");
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
        let compat = cfg
            .openai_compatible
            .expect("openai-compatible config should be present");

        assert_eq!(compat.model, "openai/gpt-5.1-codex");
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
        let compat = cfg
            .openai_compatible
            .expect("openai-compatible config should be present");

        assert_eq!(compat.model, "openai/gpt-5-codex");

        // SAFETY: Under ENV_MUTEX.
        unsafe {
            std::env::remove_var("LLM_MODEL");
        }
    }

    #[test]
    fn openai_codex_uses_auth_file_and_selected_model() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_codex_env();

        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            r#"{"tokens":{"access_token":"tok-123","account_id":"acct-456"}}"#,
        )
        .expect("write auth file");

        unsafe {
            std::env::set_var("OPENAI_CODEX_AUTH_PATH", auth_path.as_os_str());
        }

        let settings = Settings {
            llm_backend: Some("openai_codex".to_string()),
            selected_model: Some("gpt-5.2-codex".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let codex = cfg.openai_codex.expect("codex config should be present");
        assert_eq!(codex.model, "gpt-5.2-codex");
        assert_eq!(codex.base_url, "https://chatgpt.com/backend-api/codex");
        assert_eq!(codex.account_id.as_deref(), Some("acct-456"));

        unsafe {
            std::env::remove_var("OPENAI_CODEX_AUTH_PATH");
        }
    }

    #[test]
    fn copilot_uses_env_credentials_and_selected_model() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_copilot_env();

        unsafe {
            std::env::set_var("COPILOT_TOKEN", "ghu_copilot_test_token");
            std::env::set_var("COPILOT_INTEGRATION_ID", "vscode-chat");
            std::env::set_var("COPILOT_EXTRA_HEADERS", "X-Interaction-Id:test-interaction");
        }

        let settings = Settings {
            llm_backend: Some("copilot".to_string()),
            selected_model: Some("gpt-4o".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let copilot = cfg.copilot.expect("copilot config should be present");

        assert_eq!(copilot.base_url, "https://api.githubcopilot.com");
        assert_eq!(copilot.integration_id, "vscode-chat");
        assert_eq!(copilot.model, "gpt-4o");
        assert_eq!(
            build_copilot_headers(&copilot),
            vec![
                (
                    "Copilot-Integration-Id".to_string(),
                    "vscode-chat".to_string(),
                ),
                (
                    "X-Interaction-Id".to_string(),
                    "test-interaction".to_string(),
                ),
            ]
        );

        clear_copilot_env();
    }

    #[test]
    fn openai_codex_model_env_overrides_selected_model() {
        let _guard = ENV_MUTEX.lock().expect("env mutex poisoned");
        clear_openai_codex_env();

        let dir = tempfile::tempdir().expect("tempdir");
        let auth_path = dir.path().join("auth.json");
        std::fs::write(
            &auth_path,
            r#"{"tokens":{"access_token":"tok-123","account_id":"acct-456"}}"#,
        )
        .expect("write auth file");

        unsafe {
            std::env::set_var("OPENAI_CODEX_AUTH_PATH", auth_path.as_os_str());
            std::env::set_var("OPENAI_CODEX_MODEL", "gpt-5.3-codex");
        }

        let settings = Settings {
            llm_backend: Some("openai_codex".to_string()),
            selected_model: Some("gpt-5.2-codex".to_string()),
            ..Default::default()
        };

        let cfg = LlmConfig::resolve(&settings).expect("resolve should succeed");
        let codex = cfg.openai_codex.expect("codex config should be present");
        assert_eq!(codex.model, "gpt-5.3-codex");

        unsafe {
            std::env::remove_var("OPENAI_CODEX_AUTH_PATH");
            std::env::remove_var("OPENAI_CODEX_MODEL");
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
        // Values can contain colons (e.g., URLs)
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
        let ollama = cfg.ollama.expect("ollama config should be present");

        assert_eq!(ollama.model, "llama3.2");
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
        let ollama = cfg.ollama.expect("ollama config should be present");

        assert_eq!(ollama.model, "mistral:latest");

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
        let compat = cfg
            .openai_compatible
            .expect("openai-compatible config should be present");

        assert_eq!(
            compat.model, "llama3.2",
            "model name with dot must not be truncated"
        );
    }
}
