//! OpenAI-compatible embeddings (`POST {base}/v1/embeddings`).

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

/// Largest dimensionality accepted by the profile for non-Gemini providers.
pub(crate) const MAX_DIMENSIONS: u32 = 65_536;

/// Settings of an [`OpenAiCompatibleEmbedder`].
#[derive(Debug, Clone)]
pub struct OpenAiCompatibleConfig {
    /// Server root without the `/v1/embeddings` suffix, for example
    /// `https://api.example.com`. Must not carry credentials.
    pub base_url: Url,
    /// Model id sent in the request.
    pub model: String,
    /// Number of components the model returns (or is asked to return when
    /// `send_dimensions` is set). Responses of another size are refused.
    pub dimensions: u32,
    /// Send the optional `dimensions` request field (supported by newer
    /// OpenAI models and some compatible servers; others reject it).
    pub send_dimensions: bool,
    /// Batching limits (defaults: 128 entries, 100 000 tokens per request,
    /// 8 192 tokens per input).
    pub batch: BatchLimits,
    /// Concurrency, rate, timeout and retry settings.
    pub limits: RequestLimits,
    /// Optional spending cap shared with other callers.
    pub budget: Option<Budget>,
}

impl OpenAiCompatibleConfig {
    /// Settings with default limits.
    pub fn new(base_url: Url, model: impl Into<String>, dimensions: u32) -> Self {
        Self {
            base_url,
            model: model.into(),
            dimensions,
            send_dimensions: false,
            batch: BatchLimits {
                max_entries: 128,
                max_batch_tokens: 100_000,
                max_input_tokens: 8_192,
            },
            limits: RequestLimits::default(),
            budget: None,
        }
    }
}

/// Embeds through any server that implements the OpenAI embeddings protocol.
pub struct OpenAiCompatibleEmbedder {
    profile: EmbeddingProfile,
    transport: Transport,
    endpoint: Url,
    send_dimensions: bool,
    batch: BatchLimits,
    budget: Option<Budget>,
}

impl std::fmt::Debug for OpenAiCompatibleEmbedder {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("OpenAiCompatibleEmbedder")
            .field("profile", &self.profile)
            .field("endpoint", &self.endpoint.as_str())
            .finish_non_exhaustive()
    }
}

impl OpenAiCompatibleEmbedder {
    /// Builds an embedder. `api_key` is optional (local servers often need
    /// none) and is only ever sent as `Authorization: Bearer`.
    pub fn new(
        api_key: Option<SecretString>,
        config: OpenAiCompatibleConfig,
    ) -> Result<Self, EmbedError> {
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
        let endpoint = endpoint(&config.base_url, "/v1/embeddings")?;
        let transport = Transport::new(
            ProviderKind::OpenAiCompatible.as_str(),
            &config.limits,
            api_key.as_ref().map(|k| (AuthScheme::Bearer, k)),
        )?;
        Ok(Self {
            profile: EmbeddingProfile {
                provider_kind: ProviderKind::OpenAiCompatible,
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

/// `{base}{path}` with the base's trailing slash dropped, refusing URLs that
/// carry credentials.
pub(crate) fn endpoint(base: &Url, path: &str) -> Result<Url, EmbedError> {
    if !base.username().is_empty() || base.password().is_some() {
        return Err(EmbedError::Config(
            "the base url must not contain credentials".into(),
        ));
    }
    Url::parse(&format!("{}{path}", base.as_str().trim_end_matches('/')))
        .map_err(|_| EmbedError::Config("the base url is not valid".into()))
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Item>,
    usage: Option<ResponseUsage>,
}

#[derive(Deserialize)]
struct Item {
    index: Option<usize>,
    embedding: Vec<f32>,
}

#[derive(Deserialize)]
struct ResponseUsage {
    prompt_tokens: Option<u64>,
    total_tokens: Option<u64>,
}

impl BatchSender for OpenAiCompatibleEmbedder {
    fn kind(&self) -> ProviderKind {
        ProviderKind::OpenAiCompatible
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
        let mut body = json!({
            "model": self.profile.model,
            "input": texts,
            "encoding_format": "float",
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

        let mut items = parsed.data;
        // Servers may return items out of order; `index` is authoritative.
        if items.iter().all(|i| i.index.is_some()) {
            items.sort_by_key(|i| i.index);
            let sequential = items.iter().enumerate().all(|(n, i)| i.index == Some(n));
            if !sequential {
                return Err(EmbedError::Response {
                    provider: ProviderKind::OpenAiCompatible.as_str(),
                    message: "response indices are not 0..n without gaps or duplicates".into(),
                });
            }
        }
        Ok(BatchOutput {
            vectors: items.into_iter().map(|i| i.embedding).collect(),
            tokens: parsed
                .usage
                .and_then(|u| u.prompt_tokens.or(u.total_tokens)),
            attempts: reply.attempts,
            latency: reply.latency,
        })
    }
}

impl Embedder for OpenAiCompatibleEmbedder {
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
