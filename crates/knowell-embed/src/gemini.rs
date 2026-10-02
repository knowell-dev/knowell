//! Google Gemini embeddings (`models/{model}:batchEmbedContents`).
//!
//! Verified against the official docs on 2026-10-02 (see the crate README):
//!
//! - no `task_type` for `gemini-embedding-2`; the task is part of the text
//!   (`task: code retrieval | query: ...`, `title: ... | text: ...`);
//! - one `content` per request entry, because several parts in one content
//!   are merged into a single embedding;
//! - the key travels in the `x-goog-api-key` header, never in the URL.

use secrecy::SecretString;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::budget::Budget;
use crate::embedding::DocumentInput;
use crate::error::EmbedError;
use crate::limits::{BatchLimits, RequestLimits};
use crate::pipeline::{BatchOutput, BatchSender, Embedded, Usage, documents_via, query_via};
use crate::profile::{EmbeddingProfile, INPUT_FORMAT_VERSION, ProviderKind};
use crate::transport::{AuthScheme, Transport};
use crate::{Embedder, Embedding};

/// Official API host.
const DEFAULT_BASE: &str = "https://generativelanguage.googleapis.com";

/// The model Knowell targets.
pub const GEMINI_EMBEDDING_MODEL: &str = "gemini-embedding-2";

/// Smallest accepted `output_dimensionality`.
pub const GEMINI_MIN_DIMENSIONS: u32 = 128;
/// Largest accepted `output_dimensionality` (the model's native size).
pub const GEMINI_MAX_DIMENSIONS: u32 = 3072;
/// Hard cap of entries per `batchEmbedContents` request enforced by this crate.
pub const GEMINI_MAX_BATCH_ENTRIES: usize = 100;

/// Settings of a [`GeminiEmbedder`].
#[derive(Debug, Clone)]
pub struct GeminiConfig {
    /// API host; `None` means the official endpoint. Set it only for tests
    /// and proxies. Must not carry credentials.
    pub base_url: Option<Url>,
    /// Model id, `gemini-embedding-2` by default.
    pub model: String,
    /// Output dimensionality, 128 to 3072. Recommended: 768, 1536, 3072.
    pub dimensions: u32,
    /// Batching limits. Defaults: 100 entries, 50 000 estimated tokens per
    /// request, 8 192 tokens per input (the model's input limit).
    pub batch: BatchLimits,
    /// Concurrency, rate, timeout and retry settings.
    pub limits: RequestLimits,
    /// Optional spending cap shared with other callers.
    pub budget: Option<Budget>,
}

impl Default for GeminiConfig {
    fn default() -> Self {
        Self {
            base_url: None,
            model: GEMINI_EMBEDDING_MODEL.to_owned(),
            dimensions: 768,
            batch: BatchLimits {
                max_entries: GEMINI_MAX_BATCH_ENTRIES,
                max_batch_tokens: 50_000,
                max_input_tokens: 8_192,
            },
            limits: RequestLimits::default(),
            budget: None,
        }
    }
}

/// Embeds through the Gemini REST API.
///
/// Vectors are L2-normalised by this crate regardless of dimensionality.
pub struct GeminiEmbedder {
    profile: EmbeddingProfile,
    transport: Transport,
    endpoint: Url,
    model: String,
    batch: BatchLimits,
    budget: Option<Budget>,
}

impl std::fmt::Debug for GeminiEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("GeminiEmbedder")
            .field("profile", &self.profile)
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

impl GeminiEmbedder {
    /// Builds an embedder. `api_key` is supplied by the caller and is only
    /// ever sent in the `x-goog-api-key` header.
    pub fn new(api_key: SecretString, config: GeminiConfig) -> Result<Self, EmbedError> {
        validate_model(&config.model)?;
        if !(GEMINI_MIN_DIMENSIONS..=GEMINI_MAX_DIMENSIONS).contains(&config.dimensions) {
            return Err(EmbedError::Config(format!(
                "gemini dimensions must be between {GEMINI_MIN_DIMENSIONS} and {GEMINI_MAX_DIMENSIONS}"
            )));
        }
        config.batch.validate(GEMINI_MAX_BATCH_ENTRIES)?;
        config.limits.validate(&config.batch)?;

        let base = match &config.base_url {
            Some(url) => url.as_str().trim_end_matches('/').to_owned(),
            None => DEFAULT_BASE.to_owned(),
        };
        let endpoint = Url::parse(&format!(
            "{base}/v1beta/models/{}:batchEmbedContents",
            config.model
        ))
        .map_err(|_| EmbedError::Config("the gemini base url is not valid".into()))?;
        if !endpoint.username().is_empty() || endpoint.password().is_some() {
            return Err(EmbedError::Config(
                "the gemini base url must not contain credentials".into(),
            ));
        }

        let transport = Transport::new(
            ProviderKind::Gemini.as_str(),
            &config.limits,
            Some((AuthScheme::Header("x-goog-api-key"), &api_key)),
        )?;
        Ok(Self {
            profile: EmbeddingProfile {
                provider_kind: ProviderKind::Gemini,
                model: config.model.clone(),
                dimensions: config.dimensions,
                input_format_version: INPUT_FORMAT_VERSION,
            },
            transport,
            endpoint,
            model: config.model,
            batch: config.batch,
            budget: config.budget,
        })
    }
}

fn validate_model(model: &str) -> Result<(), EmbedError> {
    let ok = !model.is_empty()
        && model
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '.' | '_'));
    if ok {
        Ok(())
    } else {
        Err(EmbedError::Config(
            "model id must be non-empty and use only letters, digits, '-', '.' and '_'".into(),
        ))
    }
}

#[derive(Deserialize)]
struct BatchResponse {
    embeddings: Vec<ContentEmbedding>,
    #[serde(rename = "usageMetadata")]
    usage_metadata: Option<UsageMetadata>,
}

#[derive(Deserialize)]
struct ContentEmbedding {
    values: Vec<f32>,
}

#[derive(Deserialize)]
struct UsageMetadata {
    #[serde(rename = "promptTokenCount")]
    prompt_token_count: Option<u64>,
}

impl BatchSender for GeminiEmbedder {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Gemini
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
        let requests: Vec<serde_json::Value> = texts
            .iter()
            .map(|text| {
                json!({
                    "model": format!("models/{}", self.model),
                    "content": { "parts": [ { "text": text } ] },
                    "outputDimensionality": self.profile.dimensions,
                })
            })
            .collect();
        let body = json!({ "requests": requests });
        let reply = self
            .transport
            .post_json(&self.endpoint, &body, estimated_tokens)
            .await?;
        let parsed: BatchResponse = self.transport.parse(&reply.body)?;
        Ok(BatchOutput {
            vectors: parsed.embeddings.into_iter().map(|e| e.values).collect(),
            tokens: parsed.usage_metadata.and_then(|u| u.prompt_token_count),
            attempts: reply.attempts,
            latency: reply.latency,
        })
    }
}

impl Embedder for GeminiEmbedder {
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
