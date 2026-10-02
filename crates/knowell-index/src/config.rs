//! Settings of the indexing engine: budgets, limits, content policy, job
//! queue and worker behaviour. Workspace and project settings (track target,
//! data policy, embedding provider, excludes) come from `knowell-config`.

use std::path::PathBuf;
use std::time::Duration;

use knowell_core::Name;
use knowell_embed::Budget;
use knowell_parse::{ChunkOptions, ParseLimits};
use knowell_source::watch::WatchOptions;
use knowell_store::jobs::Backoff;
use serde::{Deserialize, Serialize};

/// Everything the engine needs besides the store and the workspace.
#[derive(Debug, Clone)]
pub struct IndexerConfig {
    /// Root of the engine's on-disk state: lexical indexes under
    /// `lexical/<view>/<generation>/` and generation manifests under
    /// `views/<view>/`. Created when missing.
    pub data_dir: PathBuf,
    /// Organization (tenant) that workspaces are registered under. Nothing
    /// is shared across organizations.
    pub organization: Name,
    /// Size and count limits.
    pub limits: Limits,
    /// Vendored, generated and minified content.
    pub content: ContentPolicy,
    /// Chunk sizes passed to `knowell-parse`.
    pub chunking: ChunkOptions,
    /// Parse bounds passed to `knowell-parse`.
    pub parse_limits: ParseLimits,
    /// Embedding batches and budget.
    pub embedding: EmbeddingSettings,
    /// Durable job settings.
    pub jobs: JobSettings,
    /// How often [`crate::Indexer::reconcile`] runs while watching. Default
    /// 10 minutes.
    pub reconcile_interval: Duration,
    /// File-watcher debounce settings.
    pub watch: WatchOptions,
    /// Which git configuration repositories are opened with.
    pub git_config: GitConfigMode,
    /// What is kept on disk and in the store.
    pub retention: Retention,
    /// Upper bound, in bytes of text, of the in-memory artifacts one build
    /// hands from stage to stage (parsed files, embedding inputs). Larger
    /// builds recompute them in later stages instead. Default 256 MiB.
    pub build_cache_bytes: usize,
    /// Files parsed concurrently (blocking threads). Default 4.
    pub parse_parallelism: usize,
    /// Jobs [`crate::Indexer::index_workspace`] runs at once (standalone
    /// mode; a [`crate::Worker`] has its own setting). Each running T0 job
    /// holds a Tantivy writer (about 50 MB). Default 2.
    pub concurrency: usize,
}

impl IndexerConfig {
    /// Defaults for everything except where the state lives and which
    /// tenant it belongs to.
    pub fn new(data_dir: impl Into<PathBuf>, organization: Name) -> Self {
        Self {
            data_dir: data_dir.into(),
            organization,
            limits: Limits::default(),
            content: ContentPolicy::default(),
            chunking: ChunkOptions::default(),
            parse_limits: ParseLimits::default(),
            embedding: EmbeddingSettings::default(),
            jobs: JobSettings::default(),
            reconcile_interval: Duration::from_secs(600),
            watch: WatchOptions::default(),
            git_config: GitConfigMode::User,
            retention: Retention::default(),
            build_cache_bytes: 256 * 1024 * 1024,
            parse_parallelism: 4,
            concurrency: 2,
        }
    }
}

/// Size and count limits. Exceeding a count limit fails the build with a
/// message naming the limit; it never indexes a silent subset.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Limits {
    /// Files larger than this many bytes are skipped (reported as too
    /// large, never truncated). Default 1 MiB.
    pub max_file_bytes: u64,
    /// Most indexable files one view may have. Default 200 000.
    pub max_files_per_view: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            max_file_bytes: 1024 * 1024,
            max_files_per_view: 200_000,
        }
    }
}

/// How vendored, generated and minified files are treated.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentPolicy {
    /// Directory (or file) names excluded at any depth, before their content
    /// is read, in addition to the built-in sensitive-file rules and the
    /// project's own `exclude` patterns. Default `vendor`, `node_modules`,
    /// `bower_components`.
    pub excluded_dirs: Vec<String>,
    /// Treatment of files `knowell-parse` flags as generated or minified.
    pub generated: GeneratedPolicy,
}

impl Default for ContentPolicy {
    fn default() -> Self {
        Self {
            excluded_dirs: vec![
                "vendor".to_owned(),
                "node_modules".to_owned(),
                "bower_components".to_owned(),
            ],
            generated: GeneratedPolicy::SkipEmbeddings,
        }
    }
}

/// Treatment of generated (`DO NOT EDIT`, lockfiles, `*.pb.go`, ...) and
/// minified files.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GeneratedPolicy {
    /// Index like any other file.
    Full,
    /// Text, symbols and chunks, but no embeddings, so generated code does
    /// not crowd semantic results or cost provider tokens (default).
    SkipEmbeddings,
    /// Text only (T0): no symbols, chunks or embeddings.
    TextOnly,
}

/// Embedding batches and budget.
#[derive(Debug, Clone)]
pub struct EmbeddingSettings {
    /// Inputs per embedding call. Default 64.
    pub batch_size: usize,
    /// Optional engine-wide token / USD cap shared by every view. A batch
    /// that does not fit is not sent; the T2 tier of that build is then
    /// `skipped(budget_exhausted)`.
    pub budget: Option<Budget>,
}

impl Default for EmbeddingSettings {
    fn default() -> Self {
        Self {
            batch_size: 64,
            budget: None,
        }
    }
}

/// Durable job settings.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobSettings {
    /// Attempts per stage job before it is dead-lettered. Default 5.
    pub max_attempts: u32,
    /// Retry delays.
    pub backoff: Backoff,
    /// Lease taken when a job is claimed; the worker renews it every third
    /// of this. Default 60 s.
    pub lease: Duration,
}

impl Default for JobSettings {
    fn default() -> Self {
        Self {
            max_attempts: 5,
            backoff: Backoff::default(),
            lease: Duration::from_secs(60),
        }
    }
}

/// Which git configuration repositories are opened with.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum GitConfigMode {
    /// System, global and repository configuration, like `git` itself.
    User,
    /// Repository configuration only, so results never depend on the
    /// machine (tests, CI).
    Isolated,
}

/// What is kept on disk and in the store.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retention {
    /// Lexical index generations kept below the active one (for readers that
    /// still hold them). Default 1.
    pub lexical_previous: usize,
    /// Store history kept per view, in generations below the active one
    /// (`None` keeps everything). Pinned manifests are always kept.
    /// Default 10.
    pub history_generations: Option<u32>,
}

impl Default for Retention {
    fn default() -> Self {
        Self {
            lexical_previous: 1,
            history_generations: Some(10),
        }
    }
}

/// Scheduling class of work. Larger runs first; interactive requests and
/// actively edited projects go before bulk indexing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Priority {
    /// Bulk and periodic work (initial indexing, reconciliation).
    Background,
    /// A project that is being edited (watcher events).
    Active,
    /// A person or agent is waiting (explicit refresh, query-triggered).
    Interactive,
}

impl Priority {
    /// Queue priority of the text, symbol and relation stages.
    pub fn value(self) -> i32 {
        match self {
            Priority::Background => 100,
            Priority::Active => 200,
            Priority::Interactive => 300,
        }
    }

    /// Queue priority of the embedding stage: below the other stages of the
    /// same class, so text and symbols of other views are not starved by
    /// slow provider calls.
    pub fn embedding_value(self) -> i32 {
        self.value() - 50
    }

    /// The class a stored queue priority belongs to.
    pub fn from_value(value: i32) -> Priority {
        if value >= Priority::Interactive.embedding_value() {
            Priority::Interactive
        } else if value >= Priority::Active.embedding_value() {
            Priority::Active
        } else {
            Priority::Background
        }
    }
}

/// Settings of a [`crate::Worker`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WorkerConfig {
    /// Jobs run at the same time. Default 2.
    pub concurrency: usize,
    /// How long an idle worker waits before polling the queue again (it is
    /// woken earlier by jobs enqueued in this process). Default 1 s.
    pub poll_interval: Duration,
}

impl Default for WorkerConfig {
    fn default() -> Self {
        Self {
            concurrency: 2,
            poll_interval: Duration::from_secs(1),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn priorities_order_and_round_trip() {
        assert!(Priority::Interactive.value() > Priority::Active.value());
        assert!(Priority::Active.value() > Priority::Background.value());
        for p in [
            Priority::Background,
            Priority::Active,
            Priority::Interactive,
        ] {
            assert!(p.embedding_value() < p.value());
            assert_eq!(Priority::from_value(p.value()), p);
            assert_eq!(Priority::from_value(p.embedding_value()), p);
        }
    }

    #[test]
    fn defaults_are_conservative() {
        let config = IndexerConfig::new("/tmp/x", Name::new("acme").unwrap());
        assert_eq!(config.limits.max_file_bytes, 1024 * 1024);
        assert!(config.content.excluded_dirs.iter().any(|d| d == "vendor"));
        assert!(
            config
                .content
                .excluded_dirs
                .iter()
                .any(|d| d == "node_modules")
        );
        assert_eq!(config.content.generated, GeneratedPolicy::SkipEmbeddings);
        assert_eq!(config.reconcile_interval, Duration::from_secs(600));
    }
}
