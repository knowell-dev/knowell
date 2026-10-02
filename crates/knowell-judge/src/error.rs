//! The error type of the judge crate.

/// Everything that can go wrong when reranking or classifying.
///
/// Messages are lowercase, actionable and never contain API keys: provider
/// error bodies and transport messages are masked before they land here.
#[derive(Debug, thiserror::Error)]
pub enum JudgeError {
    /// A cloud provider was asked to run while the data policy is local-only.
    /// Nothing was sent anywhere.
    #[error(
        "data policy is local-only: refusing to send candidates to cloud provider `{provider}`"
    )]
    PolicyRefused {
        /// Provider that refused to run.
        provider: String,
    },
    /// The provider configuration is unusable (bad URL, zero limits, ...).
    #[error("invalid judge configuration: {0}")]
    InvalidConfig(String),
    /// The call itself is malformed (empty query, `top_n` of zero, ...).
    #[error("invalid rerank request: {0}")]
    InvalidRequest(String),
    /// More candidates than the provider's configured per-call maximum. The
    /// candidates are never split across calls because scores from separate
    /// calls are not comparable.
    #[error("too many candidates: {given} given, at most {max} per call")]
    TooManyCandidates {
        /// Number of candidates supplied.
        given: usize,
        /// Configured maximum per call.
        max: usize,
    },
    /// The provider rejected the credentials (HTTP 401 or 403).
    #[error("provider rejected the credentials (http {status}): {message}")]
    Unauthorized {
        /// HTTP status code.
        status: u16,
        /// Masked, truncated provider message.
        message: String,
    },
    /// The provider kept answering 429 (or its `Retry-After` exceeded the
    /// configured cap), so retrying was abandoned.
    #[error("provider rate limit exceeded (retry after {retry_after_secs:?} s)")]
    RateLimited {
        /// Value of the `Retry-After` header in seconds, when present.
        retry_after_secs: Option<u64>,
    },
    /// The provider answered with another non-success status.
    #[error("provider returned http {status}: {message}")]
    Http {
        /// HTTP status code.
        status: u16,
        /// Masked, truncated provider message.
        message: String,
    },
    /// The request did not finish within the configured timeout.
    #[error("request to the provider timed out")]
    Timeout,
    /// The request could not be sent (connection refused, DNS, TLS, ...).
    #[error("could not reach the provider: {0}")]
    Transport(String),
    /// The provider answered 2xx with a body that does not make sense.
    #[error("invalid provider response: {0}")]
    InvalidResponse(String),
}
