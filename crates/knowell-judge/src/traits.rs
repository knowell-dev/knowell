//! The reranker and query-classifier traits.

use async_trait::async_trait;

use crate::{Candidate, DataPolicy, Descriptor, JudgeError, Scored, Usage};

/// Reorders a short candidate list by relevance to a query.
///
/// Implementations must be deterministic for a fixed [`Descriptor`] as far as
/// the backend allows, and must check `policy` before doing anything else.
#[async_trait]
pub trait Reranker: Send + Sync {
    /// Which scorer this is (provider, model, pinned version).
    fn descriptor(&self) -> Descriptor;

    /// Scores `candidates` against `query` and returns at most `top_n` of them,
    /// best first. Ties are broken by the input order, so the result is stable.
    ///
    /// `top_n` is clamped to the number of candidates; zero is an error. An
    /// empty candidate list yields an empty result without any provider call.
    async fn rerank(
        &self,
        policy: DataPolicy,
        query: &str,
        candidates: &[Candidate],
        top_n: usize,
    ) -> Result<Vec<Scored>, JudgeError>;

    /// Cumulative usage since construction. Providers without cost return zeros.
    fn usage(&self) -> Usage {
        Usage::default()
    }
}

/// Estimates which of a fixed set of labels a query belongs to (for example a
/// query intent), as a future retrieval signal.
#[async_trait]
pub trait QueryClassifier: Send + Sync {
    /// Which classifier this is.
    fn descriptor(&self) -> Descriptor;

    /// Returns one `(label, probability)` per input label, in the input order.
    /// Probabilities are in `[0, 1]` and sum to 1 (within rounding) unless
    /// `labels` is empty, in which case the result is empty.
    async fn classify(
        &self,
        policy: DataPolicy,
        query: &str,
        labels: &[String],
    ) -> Result<Vec<(String, f32)>, JudgeError>;
}
