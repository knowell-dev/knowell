//! Embedding providers and profiles: provider adapters, input
//! preparation, batching, rate limits, budgets and cache keys.
//!
//! The central pieces:
//!
//! - [`EmbeddingProfile`] identifies a vector space; vectors of different
//!   profiles must never be compared. [`cache_key`] derives cache keys that
//!   include the profile.
//! - [`Embedder`] is the async interface; [`GeminiEmbedder`],
//!   [`OpenAiCompatibleEmbedder`], [`OllamaEmbedder`] and the deterministic
//!   [`FakeEmbedder`] implement it, and [`AnyEmbedder`] wraps them for
//!   runtime selection.
//! - [`Budget`] caps tokens and USD; calls beyond it are refused whole.
//! - Every call reports [`Usage`] (tokens, requests, retries, latency).
//!
//! API keys are [`secrecy::SecretString`]s supplied by the caller. They are
//! sent only in request headers and masked out of every error message.

#![cfg_attr(
    test,
    allow(
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic,
        clippy::indexing_slicing
    )
)]

mod budget;
mod embedding;
mod error;
mod fake;
mod gemini;
mod limits;
mod ollama;
mod openai;
mod pipeline;
mod profile;
mod transport;

use std::future::Future;

pub use budget::{Budget, Reservation};
pub use embedding::{DocumentInput, Embedding, truncate_and_normalize};
pub use error::EmbedError;
pub use fake::{FAKE_MODEL, FakeEmbedder};
pub use gemini::{
    GEMINI_EMBEDDING_MODEL, GEMINI_MAX_BATCH_ENTRIES, GEMINI_MAX_DIMENSIONS, GEMINI_MIN_DIMENSIONS,
    GeminiConfig, GeminiEmbedder,
};
pub use limits::{BatchLimits, RequestLimits, RetryPolicy, estimate_tokens};
pub use ollama::{OllamaConfig, OllamaEmbedder};
pub use openai::{OpenAiCompatibleConfig, OpenAiCompatibleEmbedder};
pub use pipeline::{Embedded, Usage};
pub use profile::{
    EmbeddingProfile, GEMINI_QUERY_PREFIX, INPUT_FORMAT_VERSION, ProviderKind, cache_key,
    prepare_document, prepare_query, prepared_document_hash,
};

/// Turns text into vectors of one [`EmbeddingProfile`].
///
/// Implementations are `Send + Sync` and may be shared between tasks; the
/// per-provider concurrency limit applies across all of them. The `*_with_usage`
/// methods are the primitives; the plain methods drop the usage figures.
///
/// Documents of one call are batched, but every chunk is its own entry in
/// the request and gets its own vector, returned in input order. Inputs are
/// never truncated: an input that is too long, an empty text, or a call that
/// exceeds the budget fails the whole call before anything is sent.
pub trait Embedder: Send + Sync {
    /// The profile of every vector this embedder returns.
    fn profile(&self) -> &EmbeddingProfile;

    /// Embeds document chunks (retrieval documents, with their titles).
    fn embed_documents_with_usage(
        &self,
        documents: &[DocumentInput],
    ) -> impl Future<Output = Result<Embedded, EmbedError>> + Send;

    /// Embeds one search query.
    fn embed_query_with_usage(
        &self,
        query: &str,
    ) -> impl Future<Output = Result<(Embedding, Usage), EmbedError>> + Send;

    /// Like [`Embedder::embed_documents_with_usage`] without the usage.
    fn embed_documents(
        &self,
        documents: &[DocumentInput],
    ) -> impl Future<Output = Result<Vec<Embedding>, EmbedError>> + Send {
        async move { Ok(self.embed_documents_with_usage(documents).await?.embeddings) }
    }

    /// Like [`Embedder::embed_query_with_usage`] without the usage.
    fn embed_query(
        &self,
        query: &str,
    ) -> impl Future<Output = Result<Embedding, EmbedError>> + Send {
        async move { Ok(self.embed_query_with_usage(query).await?.0) }
    }
}

/// A runtime-selected embedder; the trait itself is not object safe.
#[derive(Debug)]
pub enum AnyEmbedder {
    /// Gemini REST API.
    Gemini(GeminiEmbedder),
    /// OpenAI-compatible server.
    OpenAiCompatible(OpenAiCompatibleEmbedder),
    /// Ollama server.
    Ollama(OllamaEmbedder),
    /// Deterministic fake.
    Fake(FakeEmbedder),
}

impl Embedder for AnyEmbedder {
    fn profile(&self) -> &EmbeddingProfile {
        match self {
            Self::Gemini(e) => e.profile(),
            Self::OpenAiCompatible(e) => e.profile(),
            Self::Ollama(e) => e.profile(),
            Self::Fake(e) => e.profile(),
        }
    }

    async fn embed_documents_with_usage(
        &self,
        documents: &[DocumentInput],
    ) -> Result<Embedded, EmbedError> {
        match self {
            Self::Gemini(e) => e.embed_documents_with_usage(documents).await,
            Self::OpenAiCompatible(e) => e.embed_documents_with_usage(documents).await,
            Self::Ollama(e) => e.embed_documents_with_usage(documents).await,
            Self::Fake(e) => e.embed_documents_with_usage(documents).await,
        }
    }

    async fn embed_query_with_usage(&self, query: &str) -> Result<(Embedding, Usage), EmbedError> {
        match self {
            Self::Gemini(e) => e.embed_query_with_usage(query).await,
            Self::OpenAiCompatible(e) => e.embed_query_with_usage(query).await,
            Self::Ollama(e) => e.embed_query_with_usage(query).await,
            Self::Fake(e) => e.embed_query_with_usage(query).await,
        }
    }
}
