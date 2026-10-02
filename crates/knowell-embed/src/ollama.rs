//! Ollama embeddings (`POST {base}/api/embed`).

use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::budget::Budget;
use crate::embedding::DocumentInput;
use crate::error::EmbedError;
use crate::limits::{BatchLimits, RequestLimits};
use crate::openai::{MAX_DIMENSIONS, endpoint};
use crate::pipeline::{BatchOutput, BatchSender, Embedded, Usage, documents_via, query_via};
use crate::profile::{EmbeddingProfile, INPUT_FORMAT_VERSION, ProviderKind};
use crate::transport::Transport;
use crate::{Embedder, Embedding};

/// Settings of an [`OllamaEmbedder`].
#[derive(Debug, Clone)]
pub struct OllamaConfig {
    /// Ollama server root, for example `http://localhost:11434`.
    pub base_url: Url,
    /// Model name, for example `nomic-embed-text`.
    pub model: String,
    /// Number of components the model returns. Responses of another size
    /// are refused.
    pub dimensions: u32,
    /// Send the optional `dimensions` field (supported by Ollama versions
    /// and models that implement Matryoshka truncation).
    pub send_dimensions: bool,
    /// Batching limits (defaults: 64 entries, 40 000 tokens per request,
    /// 2 048 tokens per input, a common local-model context size).
    pub batch: BatchLimits,
    /// Concurrency, rate, timeout and retry settings. Local models are slow
    /// on first load, so the default timeout is generous (120 s).
    pub limits: RequestLimits,
    /// Optional spending cap (tokens only are meaningful; local models are free).
    pub budget: Option<Budget>,
}

impl OllamaConfig {
    /// Settings with default limits.
    pub fn new(base_url: Url, model: impl Into<String>, dimensions: u32) -> Self {
        Self {
            base_url,
            model: model.into(),
            dimensions,
            send_dimensions: false,
            batch: BatchLimits {
                max_entries: 64,
                max_batch_tokens: 40_000,
                max_input_tokens: 2_048,
            },
            limits: RequestLimits {
                timeout: std::time::Duration::from_secs(120),
                max_concurrency: 1,
                ..RequestLimits::default()
            },
            budget: None,
        }
    }
}

/// Embeds through a local or remote Ollama server. No authentication.
pub struct OllamaEmbedder {
    profile: EmbeddingProfile,
    transport: Transport,
    endpoint: Url,
    send_dimensions: bool,
    batch: BatchLimits,
    budget: Option<Budget>,
}

impl std::fmt::Debug for OllamaEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OllamaEmbedder")
            .field("profile", &self.profile)
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

impl OllamaEmbedder {
    /// Builds an embedder.
    pub fn new(config: OllamaConfig) -> Result<Self, EmbedError> {
        if config.model.trim().is_empty() {
            return Err(EmbedError::Config("model must not be empty".into()));
        }
        if config.dimensions == 0 || config.dimensions > MAX_DIMENSIONS {
            return Err(EmbedError::Config(format!(
                "dimensions must be between 1 and {MAX_DIMENSIONS}"
            )));
        }
        config.batch.validate(2048)?;
        config.limits.validate(&config.batch)?;
        let endpoint = endpoint(&config.base_url, "/api/embed")?;
        let transport = Transport::new(ProviderKind::Ollama.as_str(), &config.limits, None)?;
        Ok(Self {
            profile: EmbeddingProfile {
                provider_kind: ProviderKind::Ollama,
                model: config.model,
                dimensions: config.dimensions,
                input_format_version: INPUT_FORMAT_VERSION,
            },
            transport,
            endpoint,
            send_dimensions: config.send_dimensions,
            batch: config.batch,
            budget: config.budget,
        })
    }
}

#[derive(Deserialize)]
struct Response {
    embeddings: Vec<Vec<f32>>,
    prompt_eval_count: Option<u64>,
}

impl BatchSender for OllamaEmbedder {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Ollama
    }

    fn dimensions(&self) -> usize {
        self.profile.dimensions as usize
    }

    fn batch_limits(&self) -> &BatchLimits {
        &self.batch
    }

    fn budget(&self) -> Option<&Budget> {
        self.budget.as_ref()
    }

    async fn send(
        &self,
        texts: &[String],
        estimated_tokens: u64,
    ) -> Result<BatchOutput, EmbedError> {
        // `truncate: false` makes Ollama fail on over-long inputs instead of
        // silently cutting them, matching our no-silent-truncation rule.
        let mut body = json!({
            "model": self.profile.model,
            "input": texts,
            "truncate": false,
        });
        if self.send_dimensions
            && let Some(map) = body.as_object_mut()
        {
            map.insert("dimensions".into(), json!(self.profile.dimensions));
        }
        let reply = self
            .transport
            .post_json(&self.endpoint, &body, estimated_tokens)
            .await?;
        let parsed: Response = self.transport.parse(&reply.body)?;
        Ok(BatchOutput {
            vectors: parsed.embeddings,
            tokens: parsed.prompt_eval_count,
            attempts: reply.attempts,
            latency: reply.latency,
        })
    }
}

impl Embedder for OllamaEmbedder {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }

    async fn embed_documents_with_usage(
        &self,
        documents: &[DocumentInput],
    ) -> Result<Embedded, EmbedError> {
        documents_via(self, documents).await
    }

    async fn embed_query_with_usage(&self, query: &str) -> Result<(Embedding, Usage), EmbedError> {
        query_via(self, query).await
    }
}
