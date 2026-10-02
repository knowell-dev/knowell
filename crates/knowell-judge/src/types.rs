//! Plain data types shared by every reranker and classifier.

use std::sync::atomic::{AtomicU64, Ordering};

/// One document offered to a reranker.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    /// Caller-chosen identifier; echoed back in [`Scored::id`]. Never sent to
    /// a provider.
    pub id: String,
    /// The text to judge (UTF-8). Truncated per provider limits before sending.
    pub text: String,
}

impl Candidate {
    /// Convenience constructor.
    pub fn new(id: impl Into<String>, text: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            text: text.into(),
        }
    }
}

/// A candidate with its relevance score.
#[derive(Clone, Debug, PartialEq)]
pub struct Scored {
    /// Identifier of the [`Candidate`].
    pub id: String,
    /// Relevance score, higher is better. The scale is provider specific and
    /// only comparable within one call.
    pub score: f32,
}

/// Identifies exactly which scorer produced a result, so scores are
/// attributable and a run can be reproduced.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Descriptor {
    /// Provider kind, e.g. `tei`, `voyage`, `cohere`, `lexical-overlap`.
    pub provider: String,
    /// Model name as configured (empty for model-less scorers).
    pub model: String,
    /// Pinned version label: the configured API/model revision, or the
    /// algorithm version for built-in scorers.
    pub version: String,
}

/// The caller's data-egress decision for one call.
///
/// Supplied by the caller (from the project's provider policy) on every call
/// so a provider can never decide on its own to send content out.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DataPolicy {
    /// When `true`, content must not leave the machine: cloud providers fail
    /// with [`crate::JudgeError::PolicyRefused`].
    pub local_only: bool,
}

impl DataPolicy {
    /// Content must stay on this machine.
    pub const LOCAL_ONLY: Self = Self { local_only: true };
    /// Cloud providers are allowed.
    pub const CLOUD_ALLOWED: Self = Self { local_only: false };
}

/// Cumulative usage of one provider instance since it was created.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Usage {
    /// HTTP requests sent, retries included.
    pub requests: u64,
    /// Documents sent, retries counted again.
    pub documents: u64,
    /// Tokens reported by the provider (Voyage `usage.total_tokens`).
    pub tokens: u64,
    /// Search units billed by the provider (Cohere `billed_units`).
    pub search_units: u64,
}

/// Lock-free counters behind [`Usage`].
#[derive(Debug, Default)]
pub(crate) struct Counters {
    requests: AtomicU64,
    documents: AtomicU64,
    tokens: AtomicU64,
    search_units: AtomicU64,
}

impl Counters {
    pub(crate) fn add_request(&self, documents: usize) {
        self.requests.fetch_add(1, Ordering::Relaxed);
        self.documents.fetch_add(
            u64::try_from(documents).unwrap_or(u64::MAX),
            Ordering::Relaxed,
        );
    }

    pub(crate) fn add_tokens(&self, n: u64) {
        self.tokens.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn add_search_units(&self, n: u64) {
        self.search_units.fetch_add(n, Ordering::Relaxed);
    }

    pub(crate) fn snapshot(&self) -> Usage {
        Usage {
            requests: self.requests.load(Ordering::Relaxed),
            documents: self.documents.load(Ordering::Relaxed),
            tokens: self.tokens.load(Ordering::Relaxed),
            search_units: self.search_units.load(Ordering::Relaxed),
        }
    }
}
