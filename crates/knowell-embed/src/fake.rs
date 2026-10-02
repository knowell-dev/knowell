//! Deterministic feature-hashing embedder for tests and CI.
//!
//! Semantics are weak by design: texts that share words, word pairs and
//! character trigrams get similar vectors. The output depends only on the
//! input text and the dimensionality, never on the platform, the process or
//! the crate versions of any RNG, so golden values stay valid everywhere.

use crate::Embedder;
use crate::budget::Budget;
use crate::embedding::{DocumentInput, Embedding};
use crate::error::EmbedError;
use crate::limits::BatchLimits;
use crate::openai::MAX_DIMENSIONS;
use crate::pipeline::{BatchOutput, BatchSender, Embedded, Usage, documents_via, query_via};
use crate::profile::{EmbeddingProfile, INPUT_FORMAT_VERSION, ProviderKind};

/// Model name reported in the profile of [`FakeEmbedder`].
pub const FAKE_MODEL: &str = "fake-hash-ngram-v1";

/// A network-free embedder that hashes word and character n-grams into a
/// fixed number of buckets.
#[derive(Debug, Clone)]
pub struct FakeEmbedder {
    profile: EmbeddingProfile,
    batch: BatchLimits,
    budget: Option<Budget>,
}

impl FakeEmbedder {
    /// Creates an embedder producing `dimensions`-component vectors.
    pub fn new(dimensions: u32) -> Result<Self, EmbedError> {
        if dimensions == 0 || dimensions > MAX_DIMENSIONS {
            return Err(EmbedError::Config(format!(
                "dimensions must be between 1 and {MAX_DIMENSIONS}"
            )));
        }
        Ok(Self {
            profile: EmbeddingProfile {
                provider_kind: ProviderKind::Fake,
                model: FAKE_MODEL.to_owned(),
                dimensions,
                input_format_version: INPUT_FORMAT_VERSION,
            },
            batch: BatchLimits {
                max_entries: 100,
                max_batch_tokens: 50_000,
                max_input_tokens: 8_192,
            },
            budget: None,
        })
    }

    /// Replaces the batching limits (useful for exercising batching in tests).
    pub fn with_batch_limits(mut self, batch: BatchLimits) -> Result<Self, EmbedError> {
        batch.validate(usize::MAX)?;
        self.batch = batch;
        Ok(self)
    }

    /// Attaches a budget, so budget behaviour can be tested without a network.
    pub fn with_budget(mut self, budget: Budget) -> Self {
        self.budget = Some(budget);
        self
    }
}

/// FNV-1a over `tag` and `bytes`, finished with a splitmix64 avalanche so
/// that both the bucket (low bits) and the sign (top bit) are well mixed.
fn feature_hash(tag: u8, bytes: &[u8]) -> u64 {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in std::iter::once(&tag).chain(bytes) {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0000_0100_0000_01b3);
    }
    h = (h ^ (h >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    h = (h ^ (h >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
    h ^ (h >> 31)
}

fn add_feature(vector: &mut [f32], tag: u8, bytes: &[u8], weight: f32) {
    let dims = vector.len() as u64;
    if dims == 0 {
        return;
    }
    let h = feature_hash(tag, bytes);
    let sign = if h >> 63 == 1 { -weight } else { weight };
    if let Some(slot) = vector.get_mut((h % dims) as usize) {
        *slot += sign;
    }
}

/// Unnormalised feature vector of `text` (never all-zero).
pub(crate) fn hash_vector(text: &str, dims: usize) -> Vec<f32> {
    let mut v = vec![0.0f32; dims];
    let lowered = text.to_lowercase();
    let words: Vec<&str> = lowered
        .split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect();

    for (i, word) in words.iter().enumerate() {
        add_feature(&mut v, b'w', word.as_bytes(), 1.0);
        let padded: Vec<char> = std::iter::once('#')
            .chain(word.chars())
            .chain(std::iter::once('#'))
            .collect();
        for tri in padded.windows(3) {
            let s: String = tri.iter().collect();
            add_feature(&mut v, b'c', s.as_bytes(), 0.3);
        }
        if let Some(next) = words.get(i + 1) {
            let pair = format!("{word} {next}");
            add_feature(&mut v, b'b', pair.as_bytes(), 0.5);
        }
    }
    if v.iter().all(|x| *x == 0.0) {
        // No usable word (empty text, only punctuation) or perfect
        // cancellation: fall back to a hash of the raw text.
        add_feature(&mut v, b'r', text.as_bytes(), 1.0);
    }
    v
}

impl BatchSender for FakeEmbedder {
    fn kind(&self) -> ProviderKind {
        ProviderKind::Fake
    }

    fn dimensions(&self) -> usize {
        self.profile.dimensions as usize
    }

    fn batch_limits(&self) -> &BatchLimits {
        &self.batch
    }

    fn budget(&self) -> Option<&Budget> {
        self.budget.as_ref()
    }

    async fn send(
        &self,
        texts: &[String],
        _estimated_tokens: u64,
    ) -> Result<BatchOutput, EmbedError> {
        let dims = self.dimensions();
        Ok(BatchOutput {
            vectors: texts.iter().map(|t| hash_vector(t, dims)).collect(),
            tokens: None,
            attempts: 1,
            latency: std::time::Duration::ZERO,
        })
    }
}

impl Embedder for FakeEmbedder {
    fn profile(&self) -> &EmbeddingProfile {
        &self.profile
    }

    async fn embed_documents_with_usage(
        &self,
        documents: &[DocumentInput],
    ) -> Result<Embedded, EmbedError> {
        documents_via(self, documents).await
    }

    async fn embed_query_with_usage(&self, query: &str) -> Result<(Embedding, Usage), EmbedError> {
        query_via(self, query).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn deterministic_across_calls_and_instances() {
        let a = FakeEmbedder::new(64).unwrap();
        let b = FakeEmbedder::new(64).unwrap();
        let docs = [DocumentInput::with_title(
            "a.rs",
            "fn parse_header(input: &str)",
        )];
        let x = a.embed_documents(&docs).await.unwrap();
        let y = b.embed_documents(&docs).await.unwrap();
        assert_eq!(x, y);
        assert_eq!(x.first().map(Embedding::dimensions), Some(64));
    }

    #[test]
    fn golden_hash_values_are_stable() {
        // Pinned so any accidental change of the hashing scheme (which would
        // silently invalidate cached fake vectors in CI) is caught.
        assert_eq!(feature_hash(b'w', b"hello"), 10_236_115_222_334_260_868);
        let expected = [0.3f32, 0.0, -0.5, -1.0, -0.7, -0.6, 0.6, 0.0];
        let actual = hash_vector("hello world", 8);
        assert_eq!(actual.len(), expected.len());
        for (a, e) in actual.iter().zip(expected) {
            assert!((a - e).abs() < 1e-5, "{actual:?}");
        }
    }

    #[tokio::test]
    async fn shared_words_are_more_similar_than_unrelated_text() {
        let e = FakeEmbedder::new(256).unwrap();
        let docs = [
            DocumentInput::new("parse the http request header and validate the token"),
            DocumentInput::new("render a pie chart with colored legend entries"),
        ];
        let vectors = e.embed_documents(&docs).await.unwrap();
        let q = e.embed_query("validate http header token").await.unwrap();
        let (Some(d0), Some(d1)) = (vectors.first(), vectors.get(1)) else {
            panic!("missing vectors");
        };
        let s0 = q.cosine(d0).unwrap();
        let s1 = q.cosine(d1).unwrap();
        assert!(s0 > s1 + 0.2, "related {s0} vs unrelated {s1}");
    }

    #[tokio::test]
    async fn output_is_unit_length_and_handles_odd_text() {
        let e = FakeEmbedder::new(32).unwrap();
        let long = "x".repeat(10_000);
        for text in ["...", "ççç ğğ", "a", "\u{1F600} emoji", long.as_str()] {
            let v = e.embed_query(text).await.unwrap();
            let norm: f32 = v.as_slice().iter().map(|x| x * x).sum();
            assert!((norm - 1.0).abs() < 1e-5, "{text}: {norm}");
        }
    }

    #[tokio::test]
    async fn rejects_empty_input_and_oversized_input() {
        let e = FakeEmbedder::new(16).unwrap();
        assert!(matches!(
            e.embed_query("  ").await,
            Err(EmbedError::InvalidInput(_))
        ));
        assert!(matches!(
            e.embed_documents(&[DocumentInput::new("")]).await,
            Err(EmbedError::InvalidInput(_))
        ));
        let huge = "word ".repeat(20_000);
        assert!(matches!(
            e.embed_documents(&[DocumentInput::new(huge)]).await,
            Err(EmbedError::InputTooLong { index: 0, .. })
        ));
    }

    #[tokio::test]
    async fn batching_and_usage_reporting() {
        let e = FakeEmbedder::new(16)
            .unwrap()
            .with_batch_limits(BatchLimits {
                max_entries: 2,
                max_batch_tokens: 1000,
                max_input_tokens: 100,
            })
            .unwrap();
        let docs: Vec<_> = (0..5)
            .map(|i| DocumentInput::new(format!("doc {i}")))
            .collect();
        let out = e.embed_documents_with_usage(&docs).await.unwrap();
        assert_eq!(out.embeddings.len(), 5);
        assert_eq!(out.usage.requests, 3);
        assert!(out.usage.tokens_estimated);
        assert!(out.usage.input_tokens > 0);
    }

    #[tokio::test]
    async fn budget_refuses_before_any_work() {
        let budget = Budget::new(Some(10), None, 0.0).unwrap();
        let e = FakeEmbedder::new(16).unwrap().with_budget(budget.clone());
        let docs = [DocumentInput::new("x".repeat(300))];
        assert!(matches!(
            e.embed_documents(&docs).await,
            Err(EmbedError::BudgetExceeded(_))
        ));
        assert_eq!(budget.spent_tokens(), 0);
    }

    #[test]
    fn configuration_is_validated() {
        assert!(FakeEmbedder::new(0).is_err());
        assert!(FakeEmbedder::new(MAX_DIMENSIONS + 1).is_err());
    }
}
