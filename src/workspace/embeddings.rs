//! Embedding providers for semantic search.
//!
//! Embeddings convert text into dense vectors that capture semantic meaning.
//! Similar concepts have similar vectors, enabling semantic search.

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

/// Error type for embedding operations.
#[derive(Debug, thiserror::Error)]
pub enum EmbeddingError {
    #[error("HTTP request failed: {0}")]
    HttpError(String),

    #[error("Invalid response: {0}")]
    InvalidResponse(String),

    #[error("Rate limited, retry after {retry_after:?}")]
    RateLimited {
        retry_after: Option<std::time::Duration>,
    },

    #[error("Authentication failed")]
    AuthFailed,

    #[error("Text too long: {length} > {max}")]
    TextTooLong { length: usize, max: usize },
}

impl From<reqwest::Error> for EmbeddingError {
    fn from(e: reqwest::Error) -> Self {
        EmbeddingError::HttpError(e.to_string())
    }
}

/// Trait for embedding providers.
#[async_trait]
pub trait EmbeddingProvider: Send + Sync {
    /// Get the embedding dimension.
    fn dimension(&self) -> usize;

    /// Get the model name.
    fn model_name(&self) -> &str;

    /// Maximum input length in characters.
    fn max_input_length(&self) -> usize;

    /// Generate an embedding for a single text.
    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError>;

    /// Generate embeddings for multiple texts (batched).
    ///
    /// Default implementation calls embed() for each text.
    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let mut embeddings = Vec::with_capacity(texts.len());
        for text in texts {
            embeddings.push(self.embed(text).await?);
        }
        Ok(embeddings)
    }
}

/// OpenAI embedding provider using text-embedding-ada-002 or text-embedding-3-small.
pub struct OpenAiEmbeddings {
    client: reqwest::Client,
    api_key: String,
    model: String,
    dimension: usize,
}

impl OpenAiEmbeddings {
    /// Create a new OpenAI embedding provider with the default model.
    ///
    /// Uses text-embedding-3-small which has 1536 dimensions.
    pub fn new(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: "text-embedding-3-small".to_string(),
            dimension: 1536,
        }
    }

    /// Use text-embedding-ada-002 model.
    pub fn ada_002(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: "text-embedding-ada-002".to_string(),
            dimension: 1536,
        }
    }

    /// Use text-embedding-3-large model.
    pub fn large(api_key: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: "text-embedding-3-large".to_string(),
            dimension: 3072,
        }
    }

    /// Use a custom model with specified dimension.
    pub fn with_model(
        api_key: impl Into<String>,
        model: impl Into<String>,
        dimension: usize,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            api_key: api_key.into(),
            model: model.into(),
            dimension,
        }
    }
}

/// OpenAI-compatible embedding provider (LM Studio, OpenRouter, etc).
///
/// Uses the OpenAI `/v1/embeddings` request/response shape.
pub struct OpenAiCompatibleEmbeddings {
    client: reqwest::Client,
    base_url: String,
    api_key: Option<String>,
    model: String,
    dimension: usize,
    max_input_chars: usize,
    max_batch_chars: usize,
}

impl OpenAiCompatibleEmbeddings {
    /// Create a new OpenAI-compatible embedding provider.
    ///
    /// `base_url` should include `/v1`, e.g. `http://localhost:1234/v1`.
    /// `api_key` is optional (LM Studio accepts any string; some providers require it).
    pub fn with_model(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        dimension: usize,
    ) -> Self {
        let model_s = model.into();
        let default_max_input_chars = if model_s.contains("nomic-embed-text") {
            8_000
        } else {
            16_000
        };
        let default_max_batch_chars = 32_000;
        Self::with_model_and_limits(
            base_url,
            api_key,
            model_s,
            dimension,
            default_max_input_chars,
            default_max_batch_chars,
        )
    }

    /// Create a new OpenAI-compatible embedding provider with explicit limits.
    pub fn with_model_and_limits(
        base_url: impl Into<String>,
        api_key: Option<String>,
        model: impl Into<String>,
        dimension: usize,
        max_input_chars: usize,
        max_batch_chars: usize,
    ) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            api_key,
            model: model.into(),
            dimension,
            max_input_chars: max_input_chars.max(256),
            max_batch_chars: max_batch_chars.max(256),
        }
    }

    fn embeddings_url(&self) -> Result<url::Url, EmbeddingError> {
        let base = self.base_url.trim_end_matches('/');
        let u = format!("{base}/embeddings");
        url::Url::parse(&u).map_err(|e| {
            EmbeddingError::InvalidResponse(format!("Invalid embeddings base_url: {e}"))
        })
    }

    async fn send_batch(
        &self,
        url: &url::Url,
        batch: &[String],
    ) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        let request = OpenAiEmbeddingRequest {
            model: &self.model,
            input: batch,
        };
        let mut req = self.client.post(url.clone()).json(&request);
        if let Some(key) = self.api_key.as_ref()
            && !key.trim().is_empty()
        {
            req = req.header("Authorization", format!("Bearer {}", key));
        }

        let response = req.send().await?;
        let status = response.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(EmbeddingError::AuthFailed);
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(std::time::Duration::from_secs);
            return Err(EmbeddingError::RateLimited { retry_after });
        }

        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(EmbeddingError::HttpError(format!(
                "Status {}: {}",
                status, error_text
            )));
        }

        let result: OpenAiEmbeddingResponse = response.json().await.map_err(|e| {
            EmbeddingError::InvalidResponse(format!("Failed to parse response: {}", e))
        })?;

        Ok(result.data.into_iter().map(|d| d.embedding).collect())
    }
}

fn clamp_chars(s: &str, max_chars: usize) -> (String, bool) {
    if s.chars().count() <= max_chars {
        return (s.to_string(), false);
    }
    let byte_offset = s
        .char_indices()
        .nth(max_chars)
        .map(|(i, _)| i)
        .unwrap_or(s.len());
    (s[..byte_offset].to_string(), true)
}

#[derive(Debug, Serialize)]
struct OpenAiEmbeddingRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingResponse {
    data: Vec<OpenAiEmbeddingData>,
}

#[derive(Debug, Deserialize)]
struct OpenAiEmbeddingData {
    embedding: Vec<f32>,
}

#[async_trait]
impl EmbeddingProvider for OpenAiEmbeddings {
    fn dimension(&self) -> usize {
        self.dimension
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn max_input_length(&self) -> usize {
        // text-embedding-3-small/large: 8191 tokens (~32k chars)
        // text-embedding-ada-002: 8191 tokens
        32_000
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if text.len() > self.max_input_length() {
            return Err(EmbeddingError::TextTooLong {
                length: text.len(),
                max: self.max_input_length(),
            });
        }

        let embeddings = self.embed_batch(&[text.to_string()]).await?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::InvalidResponse("No embedding returned".to_string()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let request = OpenAiEmbeddingRequest {
            model: &self.model,
            input: texts,
        };

        let response = self
            .client
            .post("https://api.openai.com/v1/embeddings")
            .header("Authorization", format!("Bearer {}", self.api_key))
            .json(&request)
            .send()
            .await?;

        let status = response.status();

        if status == reqwest::StatusCode::UNAUTHORIZED {
            return Err(EmbeddingError::AuthFailed);
        }

        if status == reqwest::StatusCode::TOO_MANY_REQUESTS {
            let retry_after = response
                .headers()
                .get("retry-after")
                .and_then(|v| v.to_str().ok())
                .and_then(|s| s.parse::<u64>().ok())
                .map(std::time::Duration::from_secs);
            return Err(EmbeddingError::RateLimited { retry_after });
        }

        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(EmbeddingError::HttpError(format!(
                "Status {}: {}",
                status, error_text
            )));
        }

        let result: OpenAiEmbeddingResponse = response.json().await.map_err(|e| {
            EmbeddingError::InvalidResponse(format!("Failed to parse response: {}", e))
        })?;

        Ok(result.data.into_iter().map(|d| d.embedding).collect())
    }
}

#[async_trait]
impl EmbeddingProvider for OpenAiCompatibleEmbeddings {
    fn dimension(&self) -> usize {
        self.dimension
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn max_input_length(&self) -> usize {
        self.max_input_chars
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        // Clamp instead of erroring so we don't rely on server-side truncation.
        let (clamped, truncated) = clamp_chars(text, self.max_input_length());
        if truncated {
            tracing::debug!(
                model = %self.model,
                orig_chars = text.chars().count(),
                clamped_chars = clamped.chars().count(),
                "Clamped embedding input to max chars"
            );
        }
        let embeddings = self.embed_batch(&[clamped]).await?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::InvalidResponse("No embedding returned".to_string()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        // Clamp per-input and split into smaller requests by total size.
        let mut clamped: Vec<String> = Vec::with_capacity(texts.len());
        for t in texts {
            let (c, truncated) = clamp_chars(t, self.max_input_length());
            if truncated {
                tracing::debug!(
                    model = %self.model,
                    orig_chars = t.chars().count(),
                    clamped_chars = c.chars().count(),
                    "Clamped embedding batch input to max chars"
                );
            }
            clamped.push(c);
        }

        let url = self.embeddings_url()?;
        let mut out: Vec<Vec<f32>> = Vec::with_capacity(clamped.len());

        let mut batch: Vec<String> = Vec::new();
        let mut batch_chars: usize = 0;

        for t in clamped {
            let t_chars = t.chars().count();
            if !batch.is_empty() && batch_chars + t_chars > self.max_batch_chars {
                // Flush existing batch.
                let got = self.send_batch(&url, &batch).await?;
                out.extend(got);
                batch.clear();
                batch_chars = 0;
            }
            batch_chars += t_chars;
            batch.push(t);
        }

        if !batch.is_empty() {
            let got = self.send_batch(&url, &batch).await?;
            out.extend(got);
        }

        if out.len() != texts.len() {
            return Err(EmbeddingError::InvalidResponse(format!(
                "Expected {} embeddings, got {}",
                texts.len(),
                out.len()
            )));
        }
        Ok(out)
    }
}

/// Ollama embedding provider using a local Ollama instance.
///
/// Ollama serves embedding models (e.g. `nomic-embed-text`, `mxbai-embed-large`)
/// via a REST API, typically at `http://localhost:11434`.
pub struct OllamaEmbeddings {
    client: reqwest::Client,
    base_url: String,
    model: String,
    dimension: usize,
}

impl OllamaEmbeddings {
    /// Create a new Ollama embedding provider.
    ///
    /// Defaults to `nomic-embed-text` (768 dimensions).
    pub fn new(base_url: impl Into<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            base_url: base_url.into(),
            model: "nomic-embed-text".to_string(),
            dimension: 768,
        }
    }

    /// Use a specific model with a given dimension.
    pub fn with_model(mut self, model: impl Into<String>, dimension: usize) -> Self {
        self.model = model.into();
        self.dimension = dimension;
        self
    }
}

#[derive(Debug, Serialize)]
struct OllamaEmbedRequest<'a> {
    model: &'a str,
    input: &'a [String],
}

#[derive(Debug, Deserialize)]
struct OllamaEmbedResponse {
    embeddings: Vec<Vec<f32>>,
}

#[async_trait]
impl EmbeddingProvider for OllamaEmbeddings {
    fn dimension(&self) -> usize {
        self.dimension
    }

    fn model_name(&self) -> &str {
        &self.model
    }

    fn max_input_length(&self) -> usize {
        // Most Ollama embedding models support 8192 tokens (~32k chars)
        32_000
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        if text.len() > self.max_input_length() {
            return Err(EmbeddingError::TextTooLong {
                length: text.len(),
                max: self.max_input_length(),
            });
        }

        let embeddings = self.embed_batch(&[text.to_string()]).await?;
        embeddings
            .into_iter()
            .next()
            .ok_or_else(|| EmbeddingError::InvalidResponse("No embedding returned".to_string()))
    }

    async fn embed_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>, EmbeddingError> {
        if texts.is_empty() {
            return Ok(Vec::new());
        }

        let request = OllamaEmbedRequest {
            model: &self.model,
            input: texts,
        };

        let url = format!("{}/api/embed", self.base_url);

        let response = self.client.post(&url).json(&request).send().await?;

        let status = response.status();

        if !status.is_success() {
            let error_text = response.text().await.unwrap_or_default();
            return Err(EmbeddingError::HttpError(format!(
                "Ollama returned HTTP {}: {}",
                status, error_text
            )));
        }

        let result: OllamaEmbedResponse = response.json().await.map_err(|e| {
            EmbeddingError::InvalidResponse(format!("Failed to parse Ollama response: {}", e))
        })?;

        // Validate that returned embeddings match the configured dimension.
        for (i, emb) in result.embeddings.iter().enumerate() {
            if emb.len() != self.dimension {
                return Err(EmbeddingError::InvalidResponse(format!(
                    "Ollama returned embedding of dimension {}, expected {} at index {}",
                    emb.len(),
                    self.dimension,
                    i
                )));
            }
        }

        Ok(result.embeddings)
    }
}

/// A mock embedding provider for testing.
///
/// Generates deterministic embeddings based on text hash.
/// Useful for unit and integration tests.
pub struct MockEmbeddings {
    dimension: usize,
}

impl MockEmbeddings {
    /// Create a new mock embeddings provider with the given dimension.
    pub fn new(dimension: usize) -> Self {
        Self { dimension }
    }
}

#[async_trait]
impl EmbeddingProvider for MockEmbeddings {
    fn dimension(&self) -> usize {
        self.dimension
    }

    fn model_name(&self) -> &str {
        "mock-embedding"
    }

    fn max_input_length(&self) -> usize {
        10_000
    }

    async fn embed(&self, text: &str) -> Result<Vec<f32>, EmbeddingError> {
        // Generate a deterministic embedding based on text hash
        use std::hash::{Hash, Hasher};
        let mut hasher = std::collections::hash_map::DefaultHasher::new();
        text.hash(&mut hasher);
        let hash = hasher.finish();

        let mut embedding = Vec::with_capacity(self.dimension);
        let mut seed = hash;
        for _ in 0..self.dimension {
            // Simple LCG for deterministic random values
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            let value = (seed as f32 / u64::MAX as f32) * 2.0 - 1.0;
            embedding.push(value);
        }

        // Normalize to unit length
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        if magnitude > 0.0 {
            for x in &mut embedding {
                *x /= magnitude;
            }
        }

        Ok(embedding)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_mock_embeddings() {
        let provider = MockEmbeddings::new(128);

        let embedding = provider.embed("hello world").await.unwrap();
        assert_eq!(embedding.len(), 128);

        // Check normalization (should be unit vector)
        let magnitude: f32 = embedding.iter().map(|x| x * x).sum::<f32>().sqrt();
        assert!((magnitude - 1.0).abs() < 0.001);
    }

    #[tokio::test]
    async fn test_mock_embeddings_deterministic() {
        let provider = MockEmbeddings::new(64);

        let emb1 = provider.embed("test").await.unwrap();
        let emb2 = provider.embed("test").await.unwrap();

        // Same input should produce same embedding
        assert_eq!(emb1, emb2);
    }

    #[tokio::test]
    async fn test_mock_embeddings_batch() {
        let provider = MockEmbeddings::new(64);

        let texts = vec!["hello".to_string(), "world".to_string()];
        let embeddings = provider.embed_batch(&texts).await.unwrap();

        assert_eq!(embeddings.len(), 2);
        assert_eq!(embeddings[0].len(), 64);
        assert_eq!(embeddings[1].len(), 64);

        // Different texts should produce different embeddings
        assert_ne!(embeddings[0], embeddings[1]);
    }

    #[test]
    fn test_openai_embeddings_config() {
        let provider = OpenAiEmbeddings::new("test-key");
        assert_eq!(provider.dimension(), 1536);
        assert_eq!(provider.model_name(), "text-embedding-3-small");

        let provider = OpenAiEmbeddings::large("test-key");
        assert_eq!(provider.dimension(), 3072);
        assert_eq!(provider.model_name(), "text-embedding-3-large");
    }
}
