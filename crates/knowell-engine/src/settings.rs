//! Engine settings: search knobs, caches, context lifetime, and the optional
//! inputs some REST answers are built from (glossary, domains, evaluation
//! reports, provider prices).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use knowell_config::ServerRole;
use knowell_knowledge::AcceptancePolicy;
use knowell_query::{GlossaryEntry, SearchConfig};
use serde::{Deserialize, Serialize};

/// A business domain from configuration (`GET /api/v1/domains`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DomainConfig {
    /// Stable id.
    pub id: String,
    /// Display name.
    pub name: String,
    /// What the domain covers.
    pub description: String,
    /// Projects (by name) that implement the domain.
    #[serde(default)]
    pub projects: Vec<String>,
}

/// What the engine knows about the T3 relation stage that extracts
/// cross-project contracts (`knowell-link`). Without one, contract tools
/// answer with a `contracts_not_extracted` gap instead of an empty list.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RelationStageInfo {
    /// Name of the stage, e.g. `knowell-link`.
    pub name: String,
    /// Its version, for messages.
    pub version: String,
}

/// Settings of the [`crate::Engine`]. Every field has a documented default.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    /// Fusion, expansion and rerank knobs of the query pipeline
    /// (`knowell_query::SearchConfig`, unmeasured defaults).
    pub search: SearchConfig,
    /// Glossary entries (query word ↔ code name). Default: none.
    pub glossary: Vec<GlossaryEntry>,
    /// Business domains. Default: none (the REST answer is empty and says why).
    pub domains: Vec<DomainConfig>,
    /// A `context_id` expires after this much idle time. Default 2 h.
    pub context_ttl: Duration,
    /// Most live contexts; the least recently used is dropped beyond it.
    /// Default 1024.
    pub max_contexts: usize,
    /// Bytes of redacted file text kept in memory for snippets, fetches and
    /// chunk mapping. Default 64 MiB.
    pub text_cache_bytes: usize,
    /// Per-generation snapshots (file lists, parsed symbols, chunk terms,
    /// imports) kept in memory. Default 32.
    pub snapshot_cache: usize,
    /// Directory holding evaluation reports (`*.json` written by
    /// `knowell_eval::Report::to_json`). Default: none.
    pub eval_reports_dir: Option<PathBuf>,
    /// Price per million input tokens, in USD, by provider kind
    /// (`gemini`, `ollama`, `openai-compatible`, `fake`). Providers without
    /// a price are estimated at 0 and the estimate says so. Default: none.
    pub prices_usd_per_million_tokens: BTreeMap<String, f64>,
    /// Which records are accepted on write.
    pub acceptance: AcceptancePolicy,
    /// Role of the process (administration exists only on a hub).
    pub role: ServerRole,
    /// The contract-extraction stage, when one is installed.
    pub relation_stage: Option<RelationStageInfo>,
    /// Lines of a file returned by `fetch` or packed as one body at most.
    /// Default 2000.
    pub max_fetch_lines: u32,
    /// How often buffered tool usage is added to the store; a crash loses at
    /// most this much usage. Default 5 s.
    pub usage_flush_interval: Duration,
    /// Days of hourly tool usage kept in the store. Default 400 (the longest
    /// report, 365 days, plus a month).
    pub usage_retention_days: u32,
}

impl Default for EngineSettings {
    fn default() -> Self {
        Self {
            search: SearchConfig::default(),
            glossary: Vec::new(),
            domains: Vec::new(),
            context_ttl: Duration::from_secs(2 * 60 * 60),
            max_contexts: 1024,
            text_cache_bytes: 64 * 1024 * 1024,
            snapshot_cache: 32,
            eval_reports_dir: None,
            prices_usd_per_million_tokens: BTreeMap::new(),
            acceptance: AcceptancePolicy::default(),
            role: ServerRole::Standalone,
            // knowell-index runs knowell-link's contract linking as its
            // default T3 relation stage.
            relation_stage: Some(RelationStageInfo {
                name: "knowell-link".to_owned(),
                version: env!("CARGO_PKG_VERSION").to_owned(),
            }),
            max_fetch_lines: 2000,
            usage_flush_interval: Duration::from_secs(5),
            usage_retention_days: 400,
        }
    }
}
