//! Voyage AI rerank provider (`POST /v1/rerank`).

use async_trait::async_trait;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::common::{HttpOptions, bad_config, endpoint, finish, is_loopback, prepare};
use crate::engine::Engine;
use crate::{Candidate, DataPolicy, Descriptor, JudgeError, Reranker, Scored, Usage};

/// Default Voyage API base URL.
pub const VOYAGE_DEFAULT_BASE_URL: &str = "https://api.voyageai.com/v1";

/// Configuration of [`VoyageReranker`].
#[derive(Clone, Debug)]
pub struct VoyageConfig {
    /// API key. Never logged, never in errors, never in a URL.
    pub api_key: SecretString,
    /// Model name, e.g. `rerank-2.5` or `rerank-2.5-lite`.
    pub model: String,
    /// Pinned label recorded in the [`Descriptor`]. Voyage models are not
    /// versioned by date in the API, so record the API revision you tested.
    pub version: String,
    /// Base URL; `/rerank` is appended. Defaults to [`VOYAGE_DEFAULT_BASE_URL`].
    /// `http` is accepted only for loopback hosts (tests, proxies on this
    /// machine) so the key is never sent in clear text over a network.
    pub base_url: Url,
    /// Limits and resilience settings. Voyage allows at most 1000 documents
    /// per request, which is the default `max_documents`.
    pub options: HttpOptions,
}

impl VoyageConfig {
    /// A config for the public Voyage API.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> Result<Self, JudgeError> {
        let base_url = Url::parse(VOYAGE_DEFAULT_BASE_URL)
            .map_err(|_| bad_config("built-in voyage url is invalid"))?;
        Ok(Self {
            api_key,
            model: model.into(),
            version: "v1".into(),
            base_url,
            options: HttpOptions::default(),
        })
    }
}

/// Reranker for the Voyage `/v1/rerank` API.
///
/// Request: `{"query", "documents", "model", "top_k", "truncation": true,
/// "return_documents": false}`; response: `{"data": [{"index",
/// "relevance_score"}], "usage": {"total_tokens"}}`. Token usage is reported
/// through [`Reranker::usage`].
pub struct VoyageReranker {
    url: Url,
    model: String,
    version: String,
    key: SecretString,
    options: HttpOptions,
    engine: Engine,
}

impl std::fmt::Debug for VoyageReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("VoyageReranker")
            .field("url", &self.url.as_str())
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Response {
    data: Vec<Hit>,
    #[serde(default)]
    usage: Option<UsageBody>,
}

#[derive(Deserialize)]
struct Hit {
    index: usize,
    relevance_score: f32,
}

#[derive(Deserialize)]
struct UsageBody {
    #[serde(default)]
    total_tokens: u64,
}

impl VoyageReranker {
    /// Validates the configuration and builds the provider. No network access.
    pub fn new(config: VoyageConfig) -> Result<Self, JudgeError> {
        let url = endpoint(&config.base_url, "rerank")?;
        if url.scheme() == "http" && !is_loopback(&url) {
            return Err(bad_config(
                "base_url must use https (http is only allowed for loopback hosts)",
            ));
        }
        if config.model.trim().is_empty() {
            return Err(bad_config("model must not be empty"));
        }
        let engine = Engine::new(&config.options, Some(&config.api_key))?;
        Ok(Self {
            url,
            model: config.model,
            version: config.version,
            key: config.api_key,
            options: config.options,
            engine,
        })
    }
}

#[async_trait]
impl Reranker for VoyageReranker {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            provider: "voyage".into(),
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
        if policy.local_only {
            return Err(JudgeError::PolicyRefused {
                provider: "voyage".into(),
            });
        }
        let Some(p) = prepare(&self.options, query, candidates, top_n)? else {
            return Ok(Vec::new());
        };
        let body = json!({
            "model": self.model,
            "query": p.query,
            "documents": p.texts,
            "top_k": p.top_n,
            "truncation": true,
            "return_documents": false,
        });
        let raw = self
            .engine
            .post_json(&self.url, Some(&self.key), &body, p.texts.len())
            .await?;
        let resp: Response = serde_json::from_slice(&raw)
            .map_err(|e| JudgeError::InvalidResponse(self.engine.sanitize(&e.to_string())))?;
        if let Some(u) = resp.usage {
            self.engine.counters().add_tokens(u.total_tokens);
        }
        finish(
            candidates,
            resp.data
                .into_iter()
                .map(|h| (h.index, h.relevance_score))
                .collect(),
            p.top_n,
        )
    }

    fn usage(&self) -> Usage {
        self.engine.usage()
    }
}
