//! Closed sets of values that map to PostgreSQL enum types, plus small
//! conversion helpers shared by the repositories.

use std::fmt;
use std::fmt::Write as _;

use knowell_core::ContentHash;
use serde::{Deserialize, Serialize};

use crate::error::StoreError;

macro_rules! pg_enum {
    (
        $(#[$doc:meta])*
        $name:ident = $pg:literal {
            $( $(#[$vdoc:meta])* $variant:ident = $text:literal ),+ $(,)?
        }
    ) => {
        $(#[$doc])*
        #[derive(
            Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Debug,
            Serialize, Deserialize, sqlx::Type,
        )]
        #[sqlx(type_name = $pg, rename_all = "snake_case")]
        #[serde(rename_all = "snake_case")]
        pub enum $name {
            $( $(#[$vdoc])* $variant ),+
        }

        impl $name {
            /// Every value, in declaration order.
            pub const ALL: &'static [Self] = &[$(Self::$variant),+];

            /// The stable text form (also the database and JSON value).
            pub fn as_str(&self) -> &'static str {
                match self {
                    $(Self::$variant => $text),+
                }
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                f.write_str(self.as_str())
            }
        }

        impl std::str::FromStr for $name {
            type Err = StoreError;

            fn from_str(s: &str) -> Result<Self, Self::Err> {
                match s {
                    $($text => Ok(Self::$variant),)+
                    _ => Err(StoreError::invalid(format!(
                        "`{}` is not a valid {}", s, $pg
                    ))),
                }
            }
        }
    };
}

pg_enum!(
    /// What a source is.
    SourceKind = "source_kind" {
        /// A git repository (objects are read directly; the checkout is never changed).
        Git = "git",
        /// A plain directory without version control.
        Directory = "directory",
    }
);

pg_enum!(
    /// Kind of track target a view follows; mirrors `knowell_core::TrackTarget`.
    ViewKind = "view_kind" {
        /// Local branch.
        Branch = "branch",
        /// Remote-tracking branch.
        Remote = "remote",
        /// Tag.
        Tag = "tag",
        /// Pinned commit.
        Commit = "commit",
        /// A worktree's own `HEAD`.
        Worktree = "worktree",
    }
);

pg_enum!(
    /// Lifecycle state of a view generation or an index generation.
    GenerationState = "generation_state" {
        /// Being written; not visible as the active data.
        Building = "building",
        /// The data queries use by default.
        Active = "active",
        /// Superseded by a newer active generation; still readable until pruned.
        Retired = "retired",
        /// Abandoned; its rows were rolled back.
        Failed = "failed",
    }
);

pg_enum!(
    /// Lifecycle of a blue-green embedding profile switch.
    ProfileSwitchState = "profile_switch_state" {
        /// Embeddings are built for both profiles; the old one still serves.
        Building = "building",
        /// The target profile serves every member view.
        Active = "active",
        /// Stopped before activation; the old profile kept serving.
        Cancelled = "cancelled",
        /// Undone by a rollback switch that activated.
        RolledBack = "rolled_back",
    }
);

pg_enum!(
    /// Role of a symbol occurrence.
    OccurrenceRole = "occurrence_role" {
        /// The symbol is defined here.
        Definition = "definition",
        /// The symbol is used here.
        Reference = "reference",
    }
);

pg_enum!(
    /// Kind of graph node an edge endpoint refers to.
    NodeKind = "node_kind" {
        /// A logical symbol.
        Symbol = "symbol",
        /// A file in a project.
        File = "file",
        /// A whole project.
        Project = "project",
        /// A contract (endpoint, topic, ...) in a workspace.
        Contract = "contract",
        /// A target known only by name (unresolved or external).
        Name = "name",
    }
);

pg_enum!(
    /// How an edge was established. Never merged with [`Resolution`] into
    /// one score.
    EvidenceType = "evidence_type" {
        /// Verified by a compiler, SCIP or a language tool.
        SemanticResolved = "semantic_resolved",
        /// Derived from OpenAPI, proto, a schema or a package manifest.
        ContractDerived = "contract_derived",
        /// Seen in the source structure.
        Syntactic = "syntactic",
        /// Name, structure or pattern similarity.
        Heuristic = "heuristic",
        /// Proposed by a model; needs verification.
        ModelSuggestion = "model_suggestion",
        /// Observed at runtime in a specific version and environment.
        RuntimeObserved = "runtime_observed",
    }
);

pg_enum!(
    /// Whether an edge's target is known.
    Resolution = "edge_resolution" {
        /// Exactly one target.
        Resolved = "resolved",
        /// One of several candidate targets (one edge per candidate).
        Ambiguous = "ambiguous",
        /// The target could not be determined.
        Unresolved = "unresolved",
    }
);

pg_enum!(
    /// Kind of cross-project contract.
    ContractKind = "contract_kind" {
        /// HTTP endpoint.
        Endpoint = "endpoint",
        /// Event or message topic.
        Topic = "topic",
        /// RPC method.
        Rpc = "rpc",
        /// Database table.
        Table = "table",
        /// Environment / configuration variable name (never its value).
        EnvName = "env_name",
        /// Translation key.
        I18nKey = "i18n_key",
        /// Package (dependency).
        Package = "package",
    }
);

pg_enum!(
    /// Side a project takes in a contract.
    ContractRole = "contract_role" {
        /// Defines, serves, publishes or writes it.
        Producer = "producer",
        /// Calls, subscribes to, reads or depends on it.
        Consumer = "consumer",
    }
);

pg_enum!(
    /// State of a job in the durable queue.
    JobState = "job_state" {
        /// Waiting to be claimed (once `run_after` has passed).
        Queued = "queued",
        /// Claimed by a worker that holds a lease.
        Running = "running",
        /// Finished successfully.
        Succeeded = "succeeded",
        /// Failed; will be retried after `run_after` (claimable again).
        Failed = "failed",
        /// Failed `max_attempts` times; waits for an operator (dead letter).
        Dead = "dead",
        /// Cancelled; never runs again.
        Cancelled = "cancelled",
    }
);

pg_enum!(
    /// Kind of scope a knowledge record applies to; mirrors
    /// `knowell_knowledge::Scope`.
    KnowledgeScopeKind = "knowledge_scope_kind" {
        /// The whole organization.
        Organization = "organization",
        /// One workspace.
        Workspace = "workspace",
        /// One project.
        Project = "project",
        /// Private to one task.
        Task = "task",
        /// Private to one user.
        User = "user",
    }
);

pg_enum!(
    /// How a knowledge record came to exist; mirrors
    /// `knowell_knowledge::RecordKind`.
    KnowledgeKind = "knowledge_kind" {
        /// Derived from code by deterministic analysis; carries evidence.
        Observed = "observed",
        /// Written by a human (ADR, decision, rule).
        Human = "human",
        /// Suggested by an agent; a draft until a human accepts it.
        ModelSuggestion = "model_suggestion",
    }
);

pg_enum!(
    /// Lifecycle state of a knowledge record; mirrors
    /// `knowell_knowledge::RecordState`.
    KnowledgeState = "knowledge_state" {
        /// Waiting for review.
        Proposed = "proposed",
        /// Reviewed or auto-accepted; the only state presented as current.
        Accepted = "accepted",
        /// Turned down or retracted.
        Rejected = "rejected",
        /// Its evidence no longer matches the code.
        Stale = "stale",
        /// Replaced by a newer record.
        Superseded = "superseded",
    }
);

pg_enum!(
    /// What was done to a knowledge record (history entries); mirrors
    /// `knowell_knowledge::Action`.
    KnowledgeAction = "knowledge_action" {
        /// Created in the proposed state.
        Propose = "propose",
        /// Accepted.
        Accept = "accept",
        /// Rejected or retracted.
        Reject = "reject",
        /// Flagged as stale.
        MarkStale = "mark_stale",
        /// Confirmed still true after being stale.
        Revalidate = "revalidate",
        /// Replaced by another record.
        Supersede = "supersede",
        /// New content version.
        Edit = "edit",
        /// Pinned or unpinned.
        Pin = "pin",
    }
);

pg_enum!(
    /// Lifecycle of a task; mirrors `knowell_knowledge::TaskStatus`.
    TaskStatus = "task_status" {
        /// Created, not started.
        Open = "open",
        /// Being worked on.
        InProgress = "in_progress",
        /// Waiting on something.
        Blocked = "blocked",
        /// Finished.
        Done = "done",
        /// Dropped.
        Abandoned = "abandoned",
    }
);

pg_enum!(
    /// Kind of a persisted principal. Agents are not persisted: they act for
    /// a user (see `knowell_auth::Principal`).
    PrincipalKind = "principal_kind" {
        /// A human user.
        User = "user",
        /// A non-human service account (CI, automation).
        ServiceAccount = "service_account",
    }
);

pg_enum!(
    /// A role, ordered by privilege; mirrors `knowell_auth::Role`.
    GrantRole = "grant_role" {
        /// Reads code and memory.
        Viewer = "viewer",
        /// Viewer, plus proposes memory and writes tasks.
        Member = "member",
        /// Member, plus accepts memory and manages indexes.
        Maintainer = "maintainer",
        /// Maintainer, plus administration.
        Admin = "admin",
    }
);

pg_enum!(
    /// Coarse permission class of an API token; mirrors
    /// `knowell_auth::TokenScope`.
    ApiTokenScope = "token_scope" {
        /// Read-only actions.
        Read = "read",
        /// Proposals and task writes.
        Write = "write",
        /// Administration and review.
        Admin = "admin",
    }
);

/// Converts stored hash bytes back into a [`ContentHash`].
pub(crate) fn hash_from_bytes(bytes: &[u8]) -> Result<ContentHash, StoreError> {
    if bytes.len() != 32 {
        return Err(StoreError::Corrupt(format!(
            "content hash has {} bytes instead of 32",
            bytes.len()
        )));
    }
    let mut hex = String::with_capacity(64);
    for byte in bytes {
        // Writing to a String cannot fail.
        let _ = write!(hex, "{byte:02x}");
    }
    hex.parse()
        .map_err(|_| StoreError::Corrupt("content hash is not valid".to_owned()))
}

/// Raw bytes of a hash for binding as `bytea`.
pub(crate) fn hash_bytes(hash: &ContentHash) -> Vec<u8> {
    hash.as_bytes().to_vec()
}

/// Checks a full commit id (40 or 64 lowercase hex digits).
pub(crate) fn validate_commit(commit: &str) -> Result<(), StoreError> {
    let full_length = commit.len() == 40 || commit.len() == 64;
    let hex = commit
        .bytes()
        .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b));
    if full_length && hex {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "commit must be a full 40- or 64-digit lowercase hex id",
        ))
    }
}

/// Truncates free text (error messages) to `max` bytes on a char boundary.
pub(crate) fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while end > 0 && !text.is_char_boundary(end) {
        end -= 1;
    }
    text.get(..end).unwrap_or_default().to_owned()
}

/// Converts an unsigned count or position to a database integer.
pub(crate) fn to_i32(value: u32, what: &str) -> Result<i32, StoreError> {
    i32::try_from(value).map_err(|_| StoreError::invalid(format!("{what} {value} is too large")))
}

/// Converts an unsigned byte offset or size to a database bigint.
pub(crate) fn to_i64(value: u64, what: &str) -> Result<i64, StoreError> {
    i64::try_from(value).map_err(|_| StoreError::invalid(format!("{what} {value} is too large")))
}

/// Converts a stored integer back to an unsigned value.
pub(crate) fn from_i32(value: i32, what: &str) -> Result<u32, StoreError> {
    u32::try_from(value).map_err(|_| StoreError::Corrupt(format!("{what} {value} is negative")))
}

/// Converts a stored bigint back to an unsigned value.
pub(crate) fn from_i64(value: i64, what: &str) -> Result<u64, StoreError> {
    u64::try_from(value).map_err(|_| StoreError::Corrupt(format!("{what} {value} is negative")))
}

/// Checks free text for a text column: at most `max` bytes, no NUL (which
/// PostgreSQL text cannot hold), and non-empty unless `allow_empty`. The
/// error names the field, never the value.
pub(crate) fn check_text(
    field: &str,
    text: &str,
    max: usize,
    allow_empty: bool,
) -> Result<(), StoreError> {
    if text.is_empty() && !allow_empty {
        return Err(StoreError::invalid(format!("{field} must not be empty")));
    }
    if text.len() > max {
        return Err(StoreError::invalid(format!(
            "{field} must be at most {max} bytes"
        )));
    }
    if text.contains('\0') {
        return Err(StoreError::invalid(format!(
            "{field} must not contain NUL characters"
        )));
    }
    Ok(())
}

/// Checks an identifier-like label (user key, view key, symbol id): 1 to
/// `max` bytes, no control characters, no surrounding whitespace.
pub(crate) fn check_label(field: &str, text: &str, max: usize) -> Result<(), StoreError> {
    check_text(field, text, max, false)?;
    if text.trim() != text || text.chars().any(char::is_control) {
        return Err(StoreError::invalid(format!(
            "{field} must not contain control characters or surrounding whitespace"
        )));
    }
    Ok(())
}

/// Checks that a JSON value is an array (list-valued jsonb columns).
pub(crate) fn check_json_array(field: &str, value: &serde_json::Value) -> Result<(), StoreError> {
    if value.is_array() {
        Ok(())
    } else {
        Err(StoreError::invalid(format!("{field} must be a JSON array")))
    }
}

/// Converts a stored revision counter back to an unsigned value.
pub(crate) fn from_revision(value: i64) -> Result<u64, StoreError> {
    from_i64(value, "revision")
}

/// Converts an expected revision to the database type.
pub(crate) fn to_revision(value: u64) -> Result<i64, StoreError> {
    to_i64(value, "revision")
}

/// Rejects generation numbers that cannot exist.
pub(crate) fn validate_generation(generation: i64) -> Result<(), StoreError> {
    if generation > 0 {
        Ok(())
    } else {
        Err(StoreError::invalid(format!(
            "generation must be positive, got {generation}"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn enums_round_trip_through_text() {
        for v in EvidenceType::ALL {
            assert_eq!(v.as_str().parse::<EvidenceType>().unwrap(), *v);
        }
        for v in JobState::ALL {
            assert_eq!(v.to_string().parse::<JobState>().unwrap(), *v);
        }
        assert!("nope".parse::<ContractKind>().is_err());
        assert_eq!(
            serde_json::to_string(&EvidenceType::SemanticResolved).unwrap(),
            "\"semantic_resolved\""
        );
    }

    #[test]
    fn hash_bytes_round_trip() {
        let h = ContentHash::of(b"hello");
        assert_eq!(hash_from_bytes(&hash_bytes(&h)).unwrap(), h);
        assert!(matches!(
            hash_from_bytes(&[1, 2, 3]),
            Err(StoreError::Corrupt(_))
        ));
    }

    #[test]
    fn commit_validation() {
        assert!(validate_commit(&"a".repeat(40)).is_ok());
        assert!(validate_commit(&"0".repeat(64)).is_ok());
        assert!(validate_commit("abc").is_err());
        assert!(validate_commit(&"A".repeat(40)).is_err());
        assert!(validate_commit(&"g".repeat(40)).is_err());
    }

    #[test]
    fn truncate_respects_char_boundaries() {
        assert_eq!(truncate("hello", 10), "hello");
        assert_eq!(truncate("hello", 3), "hel");
        // 'ö' is two bytes; cutting inside it backs off.
        assert_eq!(truncate("aö", 2), "a");
        assert_eq!(truncate("", 0), "");
    }

    #[test]
    fn integer_conversions() {
        assert!(to_i32(u32::MAX, "line").is_err());
        assert_eq!(to_i32(7, "line").unwrap(), 7);
        assert!(to_i64(u64::MAX, "byte").is_err());
        assert!(from_i32(-1, "line").is_err());
        assert!(from_i64(-1, "size").is_err());
        assert!(validate_generation(0).is_err());
        assert!(validate_generation(1).is_ok());
    }
}
