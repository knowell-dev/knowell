//! Cohere-compatible rerank provider (`POST /v2/rerank`).

use async_trait::async_trait;
use secrecy::SecretString;
use serde::Deserialize;
use serde_json::json;
use url::Url;

use crate::common::{HttpOptions, bad_config, endpoint, finish, is_loopback, prepare};
use crate::engine::Engine;
use crate::{Candidate, DataPolicy, Descriptor, JudgeError, Reranker, Scored, Usage};

/// Default Cohere API base URL.
pub const COHERE_DEFAULT_BASE_URL: &str = "https://api.cohere.com";

/// Configuration of [`CohereReranker`].
#[derive(Clone, Debug)]
pub struct CohereConfig {
    /// API key. Never logged, never in errors, never in a URL.
    pub api_key: SecretString,
    /// Model name, e.g. `rerank-v3.5` or `rerank-v4.0-pro`.
    pub model: String,
    /// Pinned label recorded in the [`Descriptor`] (API revision you tested).
    pub version: String,
    /// Base URL; `/v2/rerank` is appended. Defaults to
    /// [`COHERE_DEFAULT_BASE_URL`]; point it at any Cohere-compatible server.
    /// `http` is accepted only for loopback hosts.
    pub base_url: Url,
    /// Per-document token cap sent as `max_tokens_per_doc`. `None` leaves the
    /// server default (4096). Client-side character truncation in `options`
    /// is applied regardless.
    pub max_tokens_per_doc: Option<u32>,
    /// Limits and resilience settings. Cohere advises at most 1000 documents
    /// per request, which is the default `max_documents`.
    pub options: HttpOptions,
}

impl CohereConfig {
    /// A config for the public Cohere API.
    pub fn new(api_key: SecretString, model: impl Into<String>) -> Result<Self, JudgeError> {
        let base_url = Url::parse(COHERE_DEFAULT_BASE_URL)
            .map_err(|_| bad_config("built-in cohere url is invalid"))?;
        Ok(Self {
            api_key,
            model: model.into(),
            version: "v2".into(),
            base_url,
            max_tokens_per_doc: None,
            options: HttpOptions::default(),
        })
    }
}

/// Reranker for the Cohere `/v2/rerank` API.
///
/// Request: `{"model", "query", "documents", "top_n", "max_tokens_per_doc"}`;
/// response: `{"results": [{"index", "relevance_score"}], "meta":
/// {"billed_units": {"search_units"}}}`. Billed search units are reported
/// through [`Reranker::usage`].
pub struct CohereReranker {
    url: Url,
    model: String,
    version: String,
    key: SecretString,
    max_tokens_per_doc: Option<u32>,
    options: HttpOptions,
    engine: Engine,
}

impl std::fmt::Debug for CohereReranker {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CohereReranker")
            .field("url", &self.url.as_str())
            .field("model", &self.model)
            .finish_non_exhaustive()
    }
}

#[derive(Deserialize)]
struct Response {
    results: Vec<Hit>,
    #[serde(default)]
    meta: Option<Meta>,
}

#[derive(Deserialize)]
struct Hit {
    index: usize,
    relevance_score: f32,
}

#[derive(Deserialize)]
struct Meta {
    #[serde(default)]
    billed_units: Option<Billed>,
}

#[derive(Deserialize)]
struct Billed {
    /// Documented as a number; accepted as float to tolerate `1.0`.
    #[serde(default)]
    search_units: Option<f64>,
}

impl CohereReranker {
    /// Validates the configuration and builds the provider. No network access.
    pub fn new(config: CohereConfig) -> Result<Self, JudgeError> {
        let url = endpoint(&config.base_url, "v2/rerank")?;
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
            max_tokens_per_doc: config.max_tokens_per_doc,
            options: config.options,
            engine,
        })
    }
}

#[async_trait]
impl Reranker for CohereReranker {
    fn descriptor(&self) -> Descriptor {
        Descriptor {
            provider: "cohere".into(),
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
                provider: "cohere".into(),
            });
        }
        let Some(p) = prepare(&self.options, query, candidates, top_n)? else {
            return Ok(Vec::new());
        };
        let mut body = json!({
            "model": self.model,
            "query": p.query,
            "documents": p.texts,
            "top_n": p.top_n,
        });
        if let (Some(cap), Some(map)) = (self.max_tokens_per_doc, body.as_object_mut()) {
            map.insert("max_tokens_per_doc".into(), json!(cap));
        }
        let raw = self
            .engine
            .post_json(&self.url, Some(&self.key), &body, p.texts.len())
            .await?;
        let resp: Response = serde_json::from_slice(&raw)
            .map_err(|e| JudgeError::InvalidResponse(self.engine.sanitize(&e.to_string())))?;
        let units = resp
            .meta
            .and_then(|m| m.billed_units)
            .and_then(|b| b.search_units);
        if let Some(units) = units.filter(|u| u.is_finite() && *u >= 0.0) {
            // Rounded up: a fractional unit is still billed.
            self.engine.counters().add_search_units(units.ceil() as u64);
        }
        finish(
            candidates,
            resp.results
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
