//! Evaluation harness: deterministic synthetic multi-repo workspaces,
//! graded query sets, retrieval metrics and reproducible reports.
//!
//! Knowell claims that agents find the right code faster than with grep
//! alone, including across projects and for paraphrased or Turkish queries.
//! This crate is the instrument that measures that claim:
//!
//! 1. [`generate`] builds a fictional company's ten-project
//!    workspace (byte-identical on every platform) and
//!    [`Fixture::write_to`] writes it to disk, optionally as git repositories
//!    with deterministic commit ids.
//! 2. [`QuerySet::builtin`] loads the graded queries judged against the
//!    fixture's hand-written core files.
//! 3. [`run`] executes any [`Retriever`] (the [`GrepRetriever`] baseline
//!    included) and computes Recall@k, MRR@10, nDCG@10 and the abstain rate,
//!    broken down by query kind and language, into a [`Report`].
//! 4. [`compare`] diffs a report against a baseline and refuses to compare
//!    runs over different fixtures or query sets.
//!
//! ```
//! use knowell_eval::{FixtureSpec, GrepRetriever, QuerySet, Scale, generate, run};
//!
//! let fixture = generate(&FixtureSpec { seed: 42, scale: Scale::Small });
//! let queries = QuerySet::builtin()?;
//! let corpus = fixture.to_corpus();
//! let grep = GrepRetriever::new(&corpus);
//! let report = run(&corpus, &queries, &[&grep], 10)?;
//! assert_eq!(report.retrievers[0].name, "grep");
//! # Ok::<(), knowell_eval::EvalError>(())
//! ```

mod bm25;
mod compare;
mod corpus;
mod error;
mod fixture;
mod metrics;
mod queries;
mod report;
mod retriever;
mod rng;
mod walked;

pub use bm25::Bm25Retriever;
pub use compare::{Comparison, MetricDelta, compare};
pub use corpus::{Corpus, CorpusDoc};
pub use error::EvalError;
pub use fixture::{
    FIXTURE_NAME, FileRole, Fixture, FixtureFile, FixtureInfo, FixtureManifest, FixtureProject,
    FixtureSpec, ManifestFile, ManifestProject, PlantedIssue, Scale, ScaleParseError, WriteOptions,
    generate, planted_issues,
};
pub use metrics::{
    RANK_CUTOFF, RECALL_CUTOFFS, discount, first_relevant_rank, gain, ndcg_at, recall_at,
    reciprocal_rank_at, round4,
};
pub use queries::{
    BUILTIN_QUERIES_TOML, Judgment, Lang, MAX_GRADE, Query, QueryKind, QuerySet, QuerySetInfo,
};
pub use report::{
    MetricSummary, QueryMetrics, QueryOutcome, REPORT_FORMAT_VERSION, Report, RetrieverReport,
    TOP_IDS_IN_REPORT, run,
};
pub use retriever::{
    GrepRetriever, MIN_WORD_CHARS, RankedDoc, Retriever, STOPWORDS_EN, STOPWORDS_TR, sort_ranked,
};
pub use walked::{WalkedCorpus, walk_fixture};
