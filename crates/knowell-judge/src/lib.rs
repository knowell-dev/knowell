//! Rerank and classification providers: optional signals applied only to
//! short candidate lists, off by default, kept only when measurement shows
//! they help.
//!
//! * [`Reranker`] / [`QueryClassifier`]: async traits.
//! * Providers: [`TeiReranker`] (TEI-style `/rerank`, local or self-hosted),
//!   [`VoyageReranker`], [`CohereReranker`].
//! * [`LexicalOverlapReranker`]: offline deterministic baseline.
//! * Every call carries a [`DataPolicy`]; cloud providers refuse to run when
//!   it is local-only, with [`JudgeError::PolicyRefused`].
//!
//! API keys are [`secrecy::SecretString`]s and are masked out of every error.

mod cohere;
mod common;
mod engine;
mod error;
mod lexical;
mod tei;
mod traits;
mod types;
mod voyage;

pub use cohere::{COHERE_DEFAULT_BASE_URL, CohereConfig, CohereReranker};
pub use common::HttpOptions;
pub use error::JudgeError;
pub use lexical::{KeywordQueryClassifier, LexicalOverlapReranker};
pub use tei::{TeiConfig, TeiReranker};
pub use traits::{QueryClassifier, Reranker};
pub use types::{Candidate, DataPolicy, Descriptor, Scored, Usage};
pub use voyage::{VOYAGE_DEFAULT_BASE_URL, VoyageConfig, VoyageReranker};
