use std::path::PathBuf;

/// Errors of the evaluation harness.
///
/// Messages are lowercase and actionable. They never contain file contents,
/// so a canary value from the fixture can never leak through an error.
#[derive(Debug, thiserror::Error)]
pub enum EvalError {
    /// A filesystem operation failed.
    #[error("cannot {action} `{path}`: {source}")]
    Io {
        /// What was attempted (`create directory`, `write`, …).
        action: &'static str,
        /// The path involved.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// The fixture output directory already contains entries.
    #[error("output directory `{0}` is not empty; choose a new or empty directory")]
    OutputNotEmpty(PathBuf),
    /// `git` could not be started.
    #[error(
        "git is not installed or not on PATH; install git or write the fixture with `git: false`"
    )]
    GitNotFound,
    /// A `git` command exited unsuccessfully.
    #[error("`git {args}` failed in project `{project}`: {detail}")]
    Git {
        /// Project directory the command ran in.
        project: String,
        /// The git arguments (configuration overrides omitted).
        args: String,
        /// Exit status and trimmed standard error.
        detail: String,
    },
    /// The query-set file is malformed (syntax, unknown fields, wrong version).
    #[error("invalid query set: {0}")]
    QuerySet(String),
    /// One query violates a rule of the query-set format.
    #[error("query `{query}`: {reason}")]
    InvalidQuery {
        /// Query id (or `#<index>` when the id itself is invalid).
        query: String,
        /// What is wrong.
        reason: String,
    },
    /// Two corpus documents share an id.
    #[error("duplicate document id `{0}` in corpus")]
    DuplicateDoc(String),
    /// A retriever failed or broke the retriever contract.
    #[error("retriever `{retriever}` failed: {message}")]
    Retriever {
        /// Retriever name.
        retriever: String,
        /// What went wrong.
        message: String,
    },
    /// Two retrievers in one run share a name.
    #[error("retriever name `{0}` is used twice; names identify report rows")]
    DuplicateRetriever(String),
    /// The retrieval depth is below the deepest metric cut-off.
    #[error("retrieval depth must be at least {min} (the deepest metric cut-off), got {depth}")]
    DepthTooSmall {
        /// Requested depth.
        depth: usize,
        /// Minimum accepted depth.
        min: usize,
    },
    /// Two reports did not measure the same fixture and query set.
    #[error("reports are not comparable: {0}")]
    Incomparable(String),
    /// A comparison tolerance was negative or not finite.
    #[error("tolerance must be a finite number >= 0, got {0}")]
    InvalidTolerance(f64),
    /// Serialising a report or manifest failed.
    #[error("cannot serialise {what}: {message}")]
    Serialize {
        /// What was being serialised.
        what: &'static str,
        /// Serializer message.
        message: String,
    },
    /// A report could not be parsed.
    #[error("cannot parse report: {0}")]
    ReportParse(String),
    /// Walking a written fixture failed.
    #[error("cannot walk project `{project}`: {message}")]
    Walk {
        /// Project directory name.
        project: String,
        /// Walker error message (never file contents).
        message: String,
    },
    /// A fixture canary survived the secret boundary and reached the corpus.
    /// The value itself is never included.
    #[error(
        "secret boundary violated: a fixture canary reached corpus document `{doc}` (value not shown)"
    )]
    CanaryLeak {
        /// Corpus document id that contained the canary.
        doc: String,
    },
}
