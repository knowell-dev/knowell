//! The error type of the embedding crate.
//!
//! Messages are lowercase, actionable and never contain secret values:
//! provider bodies are masked with the known API key and truncated before
//! they reach an error (see `transport`).

/// Everything that can go wrong while preparing or running an embedding call.
#[derive(Debug, thiserror::Error)]
pub enum EmbedError {
    /// A provider or limit setting is invalid; nothing was sent.
    #[error("invalid embedding configuration: {0}")]
    Config(String),

    /// The caller supplied input the provider cannot embed; nothing was sent.
    #[error("invalid embedding input: {0}")]
    InvalidInput(String),

    /// An input is estimated to exceed the model's per-input token limit.
    /// Inputs are never truncated silently; split the chunk instead.
    #[error(
        "input {index} is about {estimated_tokens} tokens, above the {limit} token limit; split it before embedding"
    )]
    InputTooLong {
        /// Position of the offending input in the call.
        index: usize,
        /// Conservative token estimate (see [`crate::estimate_tokens`]).
        estimated_tokens: u64,
        /// The configured per-input limit in tokens.
        limit: u64,
    },

    /// The provider rejected the credentials (HTTP 401 or 403).
    #[error("{provider} rejected the credentials (http {status}); check the configured api key")]
    Auth {
        /// Provider name, for example `gemini`.
        provider: &'static str,
        /// HTTP status code.
        status: u16,
    },

    /// The provider answered with a non-retryable (or retries-exhausted) HTTP error.
    #[error("{provider} returned http {status}: {body}")]
    Http {
        /// Provider name.
        provider: &'static str,
        /// HTTP status code.
        status: u16,
        /// Masked and truncated response body.
        body: String,
    },

    /// The provider kept answering 429/503 with a rate-limit signal, or asked
    /// to wait longer than the configured maximum.
    #[error(
        "{provider} rate limit (http {status}) persisted after {attempts} attempt(s); retry after {retry_after_secs:?} seconds"
    )]
    RateLimited {
        /// Provider name.
        provider: &'static str,
        /// HTTP status code.
        status: u16,
        /// Attempts made, including the first.
        attempts: u32,
        /// Value of the `Retry-After` header in seconds, when present.
        retry_after_secs: Option<u64>,
    },

    /// The request could not be delivered (DNS, connect, TLS, reset).
    #[error("network error talking to {provider} after {attempts} attempt(s): {message}")]
    Network {
        /// Provider name.
        provider: &'static str,
        /// Attempts made, including the first.
        attempts: u32,
        /// Masked and truncated description.
        message: String,
    },

    /// Every attempt timed out.
    #[error("request to {provider} timed out after {attempts} attempt(s)")]
    Timeout {
        /// Provider name.
        provider: &'static str,
        /// Attempts made, including the first.
        attempts: u32,
    },

    /// The provider answered successfully but the payload is unusable.
    #[error("malformed response from {provider}: {message}")]
    Response {
        /// Provider name.
        provider: &'static str,
        /// Masked and truncated description.
        message: String,
    },

    /// The call would exceed the configured token or USD budget; nothing was sent.
    #[error("embedding budget exceeded: {0}")]
    BudgetExceeded(String),

    /// A vector operation was asked for something impossible (for example
    /// growing a vector through truncation).
    #[error("invalid vector operation: {0}")]
    Vector(String),
}
