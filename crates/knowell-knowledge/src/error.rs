//! The single error type of this crate.

use crate::model::{Action, RecordState};
use crate::task::TaskStatus;

/// Everything that can go wrong in the knowledge domain.
///
/// Messages are lowercase and actionable. None of them ever contains a secret
/// value: [`KnowledgeError::SecretDetected`] names only the field, the finding
/// kind and the line.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum KnowledgeError {
    /// An identifier failed validation.
    #[error("invalid {kind}: {reason}")]
    InvalidId {
        /// Which identifier (for example `user id`).
        kind: &'static str,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A subject key could not be normalised.
    #[error("invalid subject: {0}")]
    InvalidSubject(&'static str),
    /// A required text field is empty.
    #[error("{0} must not be empty")]
    EmptyField(&'static str),
    /// A field has an unacceptable shape.
    #[error("{field} is invalid: {reason}")]
    InvalidField {
        /// The field name.
        field: &'static str,
        /// Why it was rejected.
        reason: &'static str,
    },
    /// A field exceeds its size limit.
    #[error("{field} is too long (limit {limit})")]
    TooLong {
        /// The field name.
        field: &'static str,
        /// The limit, in bytes or items as documented on the field.
        limit: usize,
    },
    /// An observed record was written without evidence.
    #[error("observed records need at least one evidence entry")]
    MissingEvidence,
    /// The state machine does not allow this action from the current state.
    #[error("cannot {action} a record that is {from}")]
    IllegalTransition {
        /// The attempted action.
        action: Action,
        /// The state the record is in.
        from: RecordState,
    },
    /// The actor lacks the right to perform the action.
    #[error("not allowed to {action}: {reason}")]
    NotAuthorized {
        /// The attempted action.
        action: Action,
        /// Why the actor may not do it.
        reason: &'static str,
    },
    /// Text contains something that looks like a secret.
    #[error("{field} contains a secret of kind `{kind}` at line {line}; remove it and retry")]
    SecretDetected {
        /// The field that was rejected.
        field: &'static str,
        /// The finding kind (`github_token`, ...). Never the value.
        kind: &'static str,
        /// 1-based line of the finding within the field.
        line: u32,
    },
    /// Optimistic concurrency check failed.
    #[error("version conflict: expected version {expected}, record is at version {actual}")]
    VersionConflict {
        /// Version the caller based the edit on.
        expected: u32,
        /// Version the record has.
        actual: u32,
    },
    /// The superseding record is not a valid replacement.
    #[error("cannot supersede: {0}")]
    InvalidSupersede(&'static str),
    /// A task status change is not allowed.
    #[error("cannot move a task from {from} to {to}")]
    IllegalTaskTransition {
        /// Current status.
        from: TaskStatus,
        /// Requested status.
        to: TaskStatus,
    },
    /// The task is finished and cannot change any more.
    #[error("task is {0} and cannot be changed")]
    TaskClosed(TaskStatus),
    /// No open question with this number exists on the task.
    #[error("unknown open question {0}")]
    UnknownQuestion(u32),
    /// A managed section in a document is malformed.
    #[error("malformed knowell markers at line {line}: {reason}")]
    MalformedMarkers {
        /// 1-based line of the problem.
        line: u32,
        /// What is wrong.
        reason: &'static str,
    },
}
