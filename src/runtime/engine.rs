use super::*;
use serde_json::{Value, json};
use crate::error::RuntimeError;
use crate::model::*;
use crate::settings::{ModelRole, ModelRoleConfig};

pub(crate) struct ResolvedModelEngine {
    pub(crate) engine: ModelEngine,
    pub(crate) model_name: String,
    pub(crate) provider_name: String,
}

pub(crate) struct ProviderPreset;

#[derive(Clone)]
pub(crate) struct EmbeddingClient {
    pub(crate) client: reqwest::Client,
    pub(crate) base_url: String,
    pub(crate) provider_name: String,
    pub(crate) model: String,
}

impl ProviderPreset {
    pub(crate) fn from_env() -> Result<ResolvedModelEngine, anyhow::Error> {
        let provider = std::env::var("BETTERCLAW_PROVIDER")
            .unwrap_or_else(|_| "local".to_string())
            .to_lowercase();
        match provider.as_str() {
            "local" | "lmstudio" | "openai_compatible" => Self::local_from_env(),
            "openrouter" => Self::openrouter_from_env(),
            "codex" => Self::codex_from_env(),
            other => anyhow::bail!("unsupported BETTERCLAW_PROVIDER '{other}'"),
        }
    }

    fn local_from_env() -> Result<ResolvedModelEngine, anyhow::Error> {
        let model_name =
            std::env::var("BETTERCLAW_MODEL").unwrap_or_else(|_| "qwen/qwen3.5-9b".to_string());
        let base_url = std::env::var("BETTERCLAW_MODEL_BASE_URL")
            .unwrap_or_else(|_| "http://localhost:1234/v1".to_string());
        let engine = OpenAiChatCompletionsEngine::new(OpenAiCompatibleConfig {
            base_url,
            provider_name: "local-openai-compatible".to_string(),
            ..OpenAiCompatibleConfig::default()
        })?;
        Ok(ResolvedModelEngine {
            engine: ModelEngine::openai_chat_completions(engine),
            model_name,
            provider_name: "local-openai-compatible".to_string(),
        })
    }

    fn openrouter_from_env() -> Result<ResolvedModelEngine, anyhow::Error> {
        let mode = std::env::var("BETTERCLAW_PROVIDER_MODE")
            .or_else(|_| std::env::var("OPENROUTER_MODE"))
            .unwrap_or_else(|_| "chat".to_string())
            .to_lowercase();
        let model_name = std::env::var("OPENROUTER_MODEL")
            .or_else(|_| std::env::var("BETTERCLAW_MODEL"))
            .unwrap_or_else(|_| "openai/gpt-4o-mini".to_string());
        let base_url = std::env::var("OPENROUTER_BASE_URL")
            .or_else(|_| std::env::var("BETTERCLAW_MODEL_BASE_URL"))
            .unwrap_or_else(|_| "https://openrouter.ai/api/v1".to_string());
        let api_key = std::env::var("OPENROUTER_API_KEY").ok();
        let mut extra_headers = Vec::new();
        if let Ok(referer) = std::env::var("OPENROUTER_HTTP_REFERER") {
            extra_headers.push(("HTTP-Referer".to_string(), referer));
        }
        if let Ok(title) = std::env::var("OPENROUTER_X_TITLE") {
            extra_headers.push(("X-Title".to_string(), title));
        }
        let config = OpenAiCompatibleConfig {
            base_url,
            provider_name: "openrouter".to_string(),
            bearer_token: api_key,
            extra_headers,
            ..OpenAiCompatibleConfig::default()
        };
        let engine = match mode.as_str() {
            "responses" => ModelEngine::openai_responses(OpenAiResponsesEngine::new(config)?),
            "chat" | "chat_completions" => {
                ModelEngine::openai_chat_completions(OpenAiChatCompletionsEngine::new(config)?)
            }
            other => anyhow::bail!("unsupported OpenRouter mode '{other}'"),
        };
        Ok(ResolvedModelEngine {
            engine,
            model_name,
            provider_name: "openrouter".to_string(),
        })
    }

    fn codex_from_env() -> Result<ResolvedModelEngine, anyhow::Error> {
        let auth_path = std::env::var("OPENAI_CODEX_AUTH_PATH")
            .unwrap_or_else(|_| default_openai_codex_auth_path());
        let (token, account_id) = load_openai_codex_credentials(&auth_path)?;
        let model_name = std::env::var("OPENAI_CODEX_MODEL")
            .or_else(|_| std::env::var("BETTERCLAW_MODEL"))
            .unwrap_or_else(|_| "gpt-5-codex".to_string());
        let base_url = std::env::var("OPENAI_CODEX_BASE_URL")
            .or_else(|_| std::env::var("BETTERCLAW_MODEL_BASE_URL"))
            .unwrap_or_else(|_| "https://chatgpt.com/backend-api/codex".to_string());
        let mut extra_headers = Vec::new();
        if let Some(account_id) = account_id {
            extra_headers.push(("ChatGPT-Account-Id".to_string(), account_id));
        }
        let engine = OpenAiResponsesEngine::new(OpenAiCompatibleConfig {
            base_url,
            provider_name: "codex".to_string(),
            bearer_token: Some(token),
            extra_headers,
            ..OpenAiCompatibleConfig::default()
        })?;
        Ok(ResolvedModelEngine {
            engine: ModelEngine::openai_responses(engine),
            model_name,
            provider_name: "codex".to_string(),
        })
    }
}

impl EmbeddingClient {
    pub(crate) fn new(role: &ModelRoleConfig) -> Result<Self, anyhow::Error> {
        let mut config = OpenAiCompatibleConfig {
            base_url: role
                .base_url
                .clone()
                .unwrap_or_else(|| "http://localhost:1234/v1".to_string()),
            provider_name: role.provider.clone(),
            extra_headers: role.extra_headers.clone(),
            ..OpenAiCompatibleConfig::default()
        };
        if let Some(env_var) = &role.api_key_env_var {
            config.bearer_token = std::env::var(env_var).ok();
        } else {
            config.bearer_token = match role.provider.as_str() {
                "openrouter" => std::env::var("OPENROUTER_API_KEY").ok(),
                "codex" => std::env::var("OPENAI_API_KEY").ok(),
                _ => std::env::var("BETTERCLAW_EMBEDDINGS_API_KEY").ok(),
            };
        }
        Ok(Self {
            client: config.build_client(false)?,
            base_url: config.base_url,
            provider_name: config.provider_name,
            model: role.model.clone(),
        })
    }

    pub(crate) async fn embed(&self, input: &str) -> Result<Vec<f32>, RuntimeError> {
        let response = self
            .client
            .post(format!(
                "{}/embeddings",
                self.base_url.trim_end_matches('/')
            ))
            .json(&json!({
                "model": self.model,
                "input": input,
            }))
            .send()
            .await
            .map_err(|error| {
                RuntimeError::ModelParse(format!(
                    "{} embeddings transport failure: {error}",
                    self.provider_name
                ))
            })?;
        let status = response.status();
        let body: Value = response.json().await.map_err(|error| {
            RuntimeError::ModelParse(format!(
                "{} embeddings decode failure: {error}",
                self.provider_name
            ))
        })?;
        if !status.is_success() {
            return Err(RuntimeError::ModelParse(format!(
                "{} embeddings returned HTTP {}",
                self.provider_name,
                status.as_u16()
            )));
        }
        let embedding = body
            .get("data")
            .and_then(Value::as_array)
            .and_then(|items| items.first())
            .and_then(|item| item.get("embedding"))
            .and_then(Value::as_array)
            .ok_or_else(|| {
                RuntimeError::ModelParse(
                    "embeddings response missing data[0].embedding".to_string(),
                )
            })?;
        let mut values = Vec::with_capacity(embedding.len());
        for value in embedding {
            values.push(value.as_f64().unwrap_or_default() as f32);
        }
        Ok(values)
    }
}

pub(crate) fn default_openai_codex_auth_path() -> String {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_default()
        .join(".codex")
        .join("auth.json")
        .display()
        .to_string()
}

pub(crate) fn load_openai_codex_credentials(
    auth_file: &str,
) -> Result<(String, Option<String>), anyhow::Error> {
    let contents = std::fs::read_to_string(auth_file)?;
    let parsed: Value = serde_json::from_str(&contents)?;
    let tokens = parsed
        .get("tokens")
        .and_then(Value::as_object)
        .ok_or_else(|| anyhow::anyhow!("Codex auth file is missing the 'tokens' object"))?;
    let access_token = tokens
        .get("access_token")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Codex auth file is missing tokens.access_token"))?;
    let account_id = tokens
        .get("account_id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned);
    Ok((access_token.to_string(), account_id))
}

pub(crate) fn env_role(role: ModelRole) -> Option<ModelRoleConfig> {
    match role {
        ModelRole::Agent => None,
        ModelRole::Compressor => {
            let provider = std::env::var("BETTERCLAW_COMPRESSOR_PROVIDER").ok()?;
            Some(ModelRoleConfig {
                role,
                provider,
                mode: std::env::var("BETTERCLAW_COMPRESSOR_MODE").ok(),
                model: std::env::var("BETTERCLAW_COMPRESSOR_MODEL")
                    .unwrap_or_else(|_| "gpt-4o-mini".to_string()),
                base_url: std::env::var("BETTERCLAW_COMPRESSOR_BASE_URL").ok(),
                api_key_env_var: std::env::var("BETTERCLAW_COMPRESSOR_API_KEY_ENV").ok(),
                extra_headers: Vec::new(),
                enabled: true,
            })
        }
        ModelRole::Embeddings => {
            let model = std::env::var("BETTERCLAW_EMBEDDINGS_MODEL").ok()?;
            Some(ModelRoleConfig {
                role,
                provider: std::env::var("BETTERCLAW_EMBEDDINGS_PROVIDER")
                    .unwrap_or_else(|_| "openai_compatible".to_string()),
                mode: None,
                model,
                base_url: std::env::var("BETTERCLAW_EMBEDDINGS_BASE_URL").ok(),
                api_key_env_var: std::env::var("BETTERCLAW_EMBEDDINGS_API_KEY_ENV").ok(),
                extra_headers: Vec::new(),
                enabled: true,
            })
        }
    }
}

