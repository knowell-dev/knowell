use crate::ids::{JobId, ViewId};
use crate::types::GenerationState;

/// Errors returned by the store.
///
/// Messages are lowercase and never contain the database URL or password:
/// connection errors are scrubbed before they are wrapped.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum StoreError {
    /// The database URL could not be used. The reason never echoes the URL.
    #[error("invalid database url: {0}")]
    InvalidUrl(&'static str),
    /// Connecting to the server failed. The message is scrubbed of the URL
    /// and of the password.
    #[error("cannot connect to the database: {0}")]
    Connect(String),
    /// Applying the embedded migrations failed.
    #[error("database migration failed: {0}")]
    Migrate(#[source] sqlx::migrate::MigrateError),
    /// A referenced record does not exist.
    #[error("{entity} not found: {key}")]
    NotFound {
        /// Kind of record, e.g. `workspace`.
        entity: &'static str,
        /// The identifying value that was looked up.
        key: String,
    },
    /// A record with the same unique key already exists.
    #[error("{entity} already exists: {key}")]
    AlreadyExists {
        /// Kind of record, e.g. `project`.
        entity: &'static str,
        /// The conflicting key.
        key: String,
    },
    /// The caller passed a value the store cannot accept.
    #[error("invalid input: {0}")]
    InvalidInput(String),
    /// Activation of a generation lost against a newer active generation
    /// (the generation fence): a late, old job can never replace newer data.
    #[error(
        "generation {generation} of view {view} is stale: generation {active} is already active"
    )]
    StaleGeneration {
        /// The view.
        view: ViewId,
        /// The generation that tried to activate or write.
        generation: i64,
        /// The generation that is active.
        active: i64,
    },
    /// The generation exists but no longer accepts writes or activation.
    #[error("generation {generation} of view {view} is {state}, not building")]
    GenerationNotBuilding {
        /// The view.
        view: ViewId,
        /// The generation.
        generation: i64,
        /// Its current state.
        state: GenerationState,
    },
    /// Only one generation per view may be building at a time.
    #[error(
        "view {view} already has generation {building} building; activate or fail it before starting another"
    )]
    GenerationBusy {
        /// The view.
        view: ViewId,
        /// The generation that is building.
        building: i64,
    },
    /// A vector does not have the dimension of its embedding profile.
    #[error("embedding profile `{profile}` expects {expected} dimensions, got {actual}")]
    DimensionMismatch {
        /// Profile name.
        profile: String,
        /// Dimensions of the profile.
        expected: u32,
        /// Dimensions supplied.
        actual: usize,
    },
    /// A profile name is already registered with different settings.
    #[error(
        "embedding profile `{name}` already exists with different settings; profiles are immutable, register the new settings under a new name"
    )]
    ProfileConflict {
        /// Profile name.
        name: String,
    },
    /// The profile's vector index is missing or invalid, so a similarity
    /// search would silently degrade to a full scan.
    #[error(
        "embedding profile `{profile}` has no valid vector index; register the profile again to build it"
    )]
    ProfileIndexMissing {
        /// Profile name.
        profile: String,
    },
    /// The worker no longer holds the job's lease.
    #[error(
        "job {job} is not running under a lease held by `{worker}`; the lease expired or the job was cancelled or finished"
    )]
    LeaseLost {
        /// The job.
        job: JobId,
        /// The worker that tried to act on it.
        worker: String,
    },
    /// An optimistic-concurrency check failed: the record was changed by
    /// someone else since the caller read it.
    #[error("{entity} {key} was changed since it was read ({detail}); reload it and retry")]
    Conflict {
        /// Kind of record, e.g. `knowledge record`.
        entity: &'static str,
        /// The record's id.
        key: String,
        /// Expected and found values, e.g.
        /// `expected version 2 revision 5, found version 3 revision 6`.
        detail: String,
    },
    /// A stored value violates an invariant the schema cannot express.
    #[error("stored data is inconsistent: {0}")]
    Corrupt(String),
    /// Any other database error.
    #[error("database error: {0}")]
    Database(#[from] sqlx::Error),
}

impl StoreError {
    pub(crate) fn not_found(entity: &'static str, key: impl ToString) -> Self {
        Self::NotFound {
            entity,
            key: key.to_string(),
        }
    }

    pub(crate) fn already_exists(entity: &'static str, key: impl ToString) -> Self {
        Self::AlreadyExists {
            entity,
            key: key.to_string(),
        }
    }

    pub(crate) fn invalid(message: impl Into<String>) -> Self {
        Self::InvalidInput(message.into())
    }
}

/// Which integrity rule a database error broke, if any.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Violation {
    Unique(Option<String>),
    ForeignKey(Option<String>),
    Check(Option<String>),
}

/// Classifies `err` as an integrity violation (with the constraint name).
pub(crate) fn violation(err: &sqlx::Error) -> Option<Violation> {
    let db = err.as_database_error()?;
    let constraint = db.constraint().map(str::to_owned);
    if db.is_unique_violation() {
        Some(Violation::Unique(constraint))
    } else if db.is_foreign_key_violation() {
        Some(Violation::ForeignKey(constraint))
    } else if db.is_check_violation() {
        Some(Violation::Check(constraint))
    } else {
        None
    }
}

/// Maps a unique violation to [`StoreError::AlreadyExists`] and a foreign
/// key violation to [`StoreError::NotFound`] of the parent.
pub(crate) fn map_write(
    err: sqlx::Error,
    entity: &'static str,
    key: impl ToString,
    parent: &'static str,
    parent_key: impl ToString,
) -> StoreError {
    match violation(&err) {
        Some(Violation::Unique(_)) => StoreError::already_exists(entity, key),
        Some(Violation::ForeignKey(_)) => StoreError::not_found(parent, parent_key),
        _ => StoreError::Database(err),
    }
}

/// Removes every occurrence of each non-empty secret from `message`.
pub(crate) fn scrub(message: &str, secrets: &[&str]) -> String {
    let mut out = message.to_owned();
    for secret in secrets {
        if !secret.is_empty() {
            out = out.replace(secret, "[redacted]");
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scrub_removes_every_occurrence() {
        let out = scrub("a SECRET b SECRET", &["SECRET", ""]);
        assert_eq!(out, "a [redacted] b [redacted]");
    }

    #[test]
    fn messages_are_lowercase() {
        let err = StoreError::GenerationBusy {
            view: ViewId(uuid::Uuid::nil()),
            building: 3,
        };
        let text = err.to_string();
        assert!(text.starts_with("view "));
        assert!(text.contains("generation 3 building"));
    }
}
