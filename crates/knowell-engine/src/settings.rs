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

/// Optional private on-disk cache of local parse products of redacted source.
/// Limits apply per organization, including unfinished temporary files.
#[derive(Debug, Clone)]
pub struct ParseProductCacheSettings {
    /// Private cache root. Products are placed under schema and organization ids.
    pub directory: PathBuf,
    /// Most serialized bytes per entry. Default 8 MiB; hard maximum 16 MiB.
    pub max_entry_bytes: usize,
    /// Most payload bytes on disk per organization. Default 128 MiB.
    pub max_total_bytes: u64,
    /// Most entry and temporary files per organization. Default 2048; maximum 65536.
    pub max_entries: usize,
}

impl ParseProductCacheSettings {
    /// Sets the private directory and conservative unmeasured size limits.
    pub fn new(directory: impl Into<PathBuf>) -> Self {
        Self {
            directory: directory.into(),
            max_entry_bytes: 8 * 1024 * 1024,
            max_total_bytes: 128 * 1024 * 1024,
            max_entries: 2048,
        }
    }
}

/// Settings of the [`crate::Engine`]. Every field has a documented default.
#[derive(Debug, Clone)]
pub struct EngineSettings {
    /// Fusion, expansion and rerank knobs of the query pipeline
    /// (`knowell_query::SearchConfig`, unmeasured defaults).
    pub search: SearchConfig,
    /// Most non-overlapping lexical spans per pinned file, from 1 through 3.
    /// Default 1; larger values are experiments and share the existing candidate quotas.
    pub lexical_spans_per_file: u8,
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
    /// Combined per-generation metadata and full snapshots kept in memory.
    /// Metadata source bodies are hydrated only for selected paths. Default 32.
    pub snapshot_cache: usize,
    /// Persisted local parse products for cold process reuse. Default: disabled.
    /// This saves local parsing only; selected source and relationship reads still occur.
    pub parse_product_cache: Option<ParseProductCacheSettings>,
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
            lexical_spans_per_file: 1,
            glossary: Vec::new(),
            domains: Vec::new(),
            context_ttl: Duration::from_secs(2 * 60 * 60),
            max_contexts: 1024,
            text_cache_bytes: 64 * 1024 * 1024,
            snapshot_cache: 32,
            parse_product_cache: None,
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

impl EngineSettings {
    pub(crate) fn validate(&self) -> Result<(), crate::EngineError> {
        if !(1..=3).contains(&self.lexical_spans_per_file) {
            return Err(crate::EngineError::Config(
                "lexical spans per file must be between 1 and 3".into(),
            ));
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexical_span_experiment_is_opt_in_and_rejects_invalid_limits() {
        assert_eq!(EngineSettings::default().lexical_spans_per_file, 1);
        for lexical_spans_per_file in 1..=3 {
            let settings = EngineSettings {
                lexical_spans_per_file,
                ..EngineSettings::default()
            };
            assert!(settings.validate().is_ok());
        }
        for lexical_spans_per_file in [0, 4, u8::MAX] {
            let settings = EngineSettings {
                lexical_spans_per_file,
                ..EngineSettings::default()
            };
            assert!(matches!(
                settings.validate(),
                Err(crate::EngineError::Config(_))
            ));
        }
    }
}
