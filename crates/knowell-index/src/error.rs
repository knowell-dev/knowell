//! The error type of the indexing engine.

use knowell_store::{StoreError, ViewId};

/// Errors of the indexing engine.
///
/// Messages are lowercase and never contain file content or secret values:
/// git, store and provider errors are already scrubbed by the crates that
/// produce them, and errors raised here name ids, refs and paths only.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum IndexError {
    /// A store operation failed.
    #[error(transparent)]
    Store(#[from] StoreError),
    /// Reading git objects or refs failed.
    #[error(transparent)]
    Git(#[from] knowell_source::git::GitError),
    /// Walking a directory source failed.
    #[error(transparent)]
    Source(#[from] knowell_source::SourceError),
    /// Starting a file watcher failed.
    #[error(transparent)]
    Watch(#[from] knowell_source::watch::WatchError),
    /// The lexical (Tantivy) index failed.
    #[error(transparent)]
    Lexical(#[from] knowell_lexical::LexicalError),
    /// An embedding call failed.
    #[error(transparent)]
    Embed(#[from] knowell_embed::EmbedError),
    /// An exclusion pattern could not be compiled.
    #[error(transparent)]
    Exclusion(#[from] knowell_secrets::SecretsError),
    /// The view is not known to this indexer.
    #[error("view {0} is not registered with this indexer; register its workspace first")]
    UnknownView(ViewId),
    /// The engine or workspace configuration cannot be used as given.
    #[error("invalid configuration: {0}")]
    Config(String),
    /// A resource limit was exceeded; the message names the limit and how to
    /// raise it. Nothing is silently truncated.
    #[error("{0}")]
    Limit(String),
    /// A file-system operation on the engine's own data failed.
    #[error("{context}: {source}")]
    Io {
        /// What was being done, e.g. `copying the lexical index`.
        context: String,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A job payload or another serialized value could not be used.
    #[error("invalid {what}: {reason}")]
    Invalid {
        /// What was invalid, e.g. `job payload`.
        what: &'static str,
        /// Why.
        reason: String,
    },
    /// The job was cancelled, its lease was lost, or the worker is shutting
    /// down.
    #[error("the job was cancelled before it finished")]
    Cancelled,
    /// A background task panicked or was aborted.
    #[error("background task failed: {0}")]
    Task(String),
    /// Stored data contradicts an invariant of the pipeline.
    #[error("index data is inconsistent: {0}")]
    Inconsistent(String),
}

impl IndexError {
    pub(crate) fn io(context: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            context: context.into(),
            source,
        }
    }

    pub(crate) fn invalid(what: &'static str, reason: impl Into<String>) -> Self {
        Self::Invalid {
            what,
            reason: reason.into(),
        }
    }

    /// Whether this error means another build of the same view replaced the
    /// one that hit it (the generation fence), so the job should end quietly
    /// instead of being retried.
    pub(crate) fn is_superseded(&self) -> bool {
        matches!(
            self,
            IndexError::Store(
                StoreError::StaleGeneration { .. } | StoreError::GenerationNotBuilding { .. }
            )
        )
    }
}

impl From<tokio::task::JoinError> for IndexError {
    fn from(error: tokio::task::JoinError) -> Self {
        if error.is_cancelled() {
            IndexError::Task("the task was aborted".to_owned())
        } else {
            IndexError::Task("the task panicked".to_owned())
        }
    }
}
