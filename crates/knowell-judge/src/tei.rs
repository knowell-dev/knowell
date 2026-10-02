//! TEI-style `/rerank` provider (Hugging Face Text Embeddings Inference and
//! compatible servers) for local or self-hosted rerank models.

use async_trait::async_trait;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::common::{HttpOptions, endpoint, finish, is_loopback, prepare};
use crate::engine::Engine;
use crate::{Candidate, DataPolicy, Descriptor, JudgeError, Reranker, Scored, Usage};

/// Configuration of [`TeiReranker`].
#[derive(Clone, Debug)]
pub struct TeiConfig {
    /// Server base URL, e.g. `http://127.0.0.1:8080`. `/rerank` is appended.
    /// Must not contain credentials or a query string.
    pub base_url: Url,
    /// Name of the model the server was started with (for example a
    /// Qwen3-Reranker or bge-reranker checkpoint). TEI does not take a model
    /// in the request, so this is only recorded in the [`Descriptor`]; keep it
    /// equal to the server's `--model-id`.
    pub model: String,
    /// Pinned revision label recorded in the [`Descriptor`] (for example the
    /// model commit hash or the TEI image tag).
    pub version: String,
    /// Optional bearer token (TEI `--api-key`).
    pub api_key: Option<SecretString>,
    /// Whether the endpoint is on this machine and therefore allowed under a
    /// local-only policy. `None` means "yes iff the host is localhost or a
    /// loopback address". Set `Some(true)` only for a trusted private network
    /// server; the library cannot verify it.
    pub assume_local: Option<bool>,
    /// Limits and resilience settings. For TEI set `max_documents` to the
    /// server's `--max-client-batch-size` (default 32).
    pub options: HttpOptions,
}

impl TeiConfig {
    /// A config with defaults: `max_documents` 32, version `unpinned`.
    pub fn new(base_url: Url, model: impl Into<String>) -> Self {
        Self {
            base_url,
            model: model.into(),
            version: "unpinned".into(),
            api_key: None,
            assume_local: None,
            options: HttpOptions {
                max_documents: 32,
                ..HttpOptions::default()
            },
        }
    }
}

/// Reranker speaking the TEI `POST /rerank` protocol.
///
/// Request: `{"query", "texts", "raw_scores": false, "return_text": false,
/// "truncate": true}`; response: `[{"index", "score"}, ...]`. Scores are the
/// model's normalised relevance (sigmoid) scores.
pub struct TeiReranker {
    url: Url,
    model: String,
    version: String,
    key: Option<SecretString>,
    is_local: bool,
    options: HttpOptions,
    engine: Engine,
}

impl std::fmt::Debug for TeiReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("TeiReranker")
            .field("url", &self.url.as_str())
            .field("model", &self.model)
            .field("has_key", &self.key.is_some())
            .finish()
    }
}

#[derive(Deserialize)]
struct Hit {
    index: usize,
    score: f32,
}

impl TeiReranker {
    /// Validates the configuration and builds the provider. No network access.
    pub fn new(config: TeiConfig) -> Result<Self, JudgeError> {
        let url = endpoint(&config.base_url, "rerank")?;
        let engine = Engine::new(&config.options, config.api_key.as_ref())?;
        let is_local = config.assume_local.unwrap_or_else(|| is_loopback(&url));
        Ok(Self {
            url,
            model: config.model,
            version: config.version,
            key: config.api_key,
            is_local,
            options: config.options,
            engine,
        })
    }
}

#[async_trait]
impl Reranker for TeiReranker {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            provider: "tei".into(),
            model: self.model.clone(),
            version: self.version.clone(),
        }
    }

    async fn rerank(
        &self,
        policy: DataPolicy,
        query: &str,
        candidates: &[Candidate],
        top_n: usize,
    ) -> Result<Vec<Scored>, JudgeError> {
        if policy.local_only && !self.is_local {
            return Err(JudgeError::PolicyRefused {
                provider: "tei".into(),
            });
        }
        let Some(p) = prepare(&self.options, query, candidates, top_n)? else {
            return Ok(Vec::new());
        };
        let body = json!({
            "query": p.query,
            "texts": p.texts,
            "raw_scores": false,
            "return_text": false,
            "truncate": true,
        });
        let raw = self
            .engine
            .post_json(&self.url, self.key.as_ref(), &body, p.texts.len())
            .await?;
        let hits: Vec<Hit> = serde_json::from_slice(&raw)
            .map_err(|e| JudgeError::InvalidResponse(self.engine.sanitize(&e.to_string())))?;
        finish(
            candidates,
            hits.into_iter().map(|h| (h.index, h.score)).collect(),
            p.top_n,
        )
    }

    fn usage(&self) -> Usage {
        self.engine.usage()
    }
}
