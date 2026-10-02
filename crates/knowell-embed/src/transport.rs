//! Shared HTTP transport: auth, concurrency, rate limits, retries, timeouts
//! and error sanitising. Every provider sends its requests through this.

use std::time::{Duration, Instant};

use knowell_secrets::Masker;
use reqwest::StatusCode;
use reqwest::header::{CONTENT_TYPE, HeaderName, HeaderValue, RETRY_AFTER};
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::Semaphore;
use url::Url;

use crate::error::EmbedError;
use crate::limits::{RequestLimits, RetryPolicy, TokenBucket};

/// Provider error bodies longer than this many characters are cut.
const MAX_ERROR_CHARS: usize = 300;

/// How the API key is attached to a request. Never the URL.
#[derive(Debug, Clone, Copy)]
pub(crate) enum AuthScheme {
    /// `{name}: {key}`, for example `x-goog-api-key`.
    Header(&'static str),
    /// `Authorization: Bearer {key}`.
    Bearer,
}

/// A successful HTTP exchange.
#[derive(Debug)]
pub(crate) struct Reply {
    pub(crate) body: Vec<u8>,
    /// Attempts made, including the first.
    pub(crate) attempts: u32,
    /// Wall time of the whole exchange including backoff waits.
    pub(crate) latency: Duration,
}

enum Failure {
    Fatal(EmbedError),
    Transient(Transient),
}

enum Transient {
    Status {
        status: u16,
        body: String,
        retry_after: Option<Duration>,
    },
    Network(String),
    Timeout,
}

impl Transient {
    fn retry_after(&self) -> Option<Duration> {
        match self {
            Self::Status { retry_after, .. } => *retry_after,
            _ => None,
        }
    }

    fn into_error(self, provider: &'static str, attempts: u32) -> EmbedError {
        match self {
            Self::Status {
                status,
                body,
                retry_after,
            } => {
                if status == 429 || retry_after.is_some() {
                    EmbedError::RateLimited {
                        provider,
                        status,
                        attempts,
                        retry_after_secs: retry_after.map(|d| d.as_secs()),
                    }
                } else {
                    EmbedError::Http {
                        provider,
                        status,
                        body,
                    }
                }
            }
            Self::Network(message) => EmbedError::Network {
                provider,
                attempts,
                message,
            },
            Self::Timeout => EmbedError::Timeout { provider, attempts },
        }
    }
}

pub(crate) struct Transport {
    provider: &'static str,
    client: reqwest::Client,
    permits: Semaphore,
    requests: Option<TokenBucket>,
    tokens: Option<TokenBucket>,
    retry: RetryPolicy,
    masker: Masker,
    auth: Option<(HeaderName, HeaderValue)>,
}

impl std::fmt::Debug for Transport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Deliberately omits the auth header and the masker.
        f.debug_struct("Transport")
            .field("provider", &self.provider)
            .field("authenticated", &self.auth.is_some())
            .finish_non_exhaustive()
    }
}

impl Transport {
    pub(crate) fn new(
        provider: &'static str,
        limits: &RequestLimits,
        auth: Option<(AuthScheme, &SecretString)>,
    ) -> Result<Self, EmbedError> {
        // Redirects are disabled: reqwest would forward custom auth headers
        // such as `x-goog-api-key` to a different host.
        let client = reqwest::Client::builder()
            .timeout(limits.timeout)
            .connect_timeout(limits.timeout)
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("knowell-embed/", env!("CARGO_PKG_VERSION")))
            .build()
            .map_err(|e| EmbedError::Config(format!("cannot build http client: {e}")))?;

        let mut masker = Masker::new();
        let auth = match auth {
            None => None,
            Some((scheme, key)) => {
                masker.add(SecretString::from(key.expose_secret().to_owned()));
                let invalid = || {
                    EmbedError::Config(
                        "the api key contains characters that are not valid in an http header"
                            .into(),
                    )
                };
                let (name, mut value) = match scheme {
                    AuthScheme::Header(name) => (
                        HeaderName::from_static(name),
                        HeaderValue::from_str(key.expose_secret()).map_err(|_| invalid())?,
                    ),
                    AuthScheme::Bearer => (
                        reqwest::header::AUTHORIZATION,
                        HeaderValue::from_str(&format!("Bearer {}", key.expose_secret()))
                            .map_err(|_| invalid())?,
                    ),
                };
                value.set_sensitive(true);
                Some((name, value))
            }
        };

        Ok(Self {
            provider,
            client,
            permits: Semaphore::new(limits.max_concurrency),
            requests: limits.requests_per_minute.map(TokenBucket::per_minute),
            tokens: limits.tokens_per_minute.map(TokenBucket::per_minute),
            retry: limits.retry,
            masker,
            auth,
        })
    }

    /// Masks the API key and truncates, so text is safe for errors and logs.
    pub(crate) fn sanitize(&self, text: &str) -> String {
        // Mask first on the full text: truncating first could split the key
        // and leave a recognisable fragment behind.
        let masked = self.masker.mask(text);
        let mut out: String = masked.chars().take(MAX_ERROR_CHARS).collect();
        if masked.chars().nth(MAX_ERROR_CHARS).is_some() {
            out.push_str("... (truncated)");
        }
        out
    }

    /// Parses a JSON response body, with sanitised errors.
    pub(crate) fn parse<T: serde::de::DeserializeOwned>(
        &self,
        body: &[u8],
    ) -> Result<T, EmbedError> {
        serde_json::from_slice(body).map_err(|e| EmbedError::Response {
            provider: self.provider,
            message: self.sanitize(&e.to_string()),
        })
    }

    /// POSTs `body` as JSON, retrying transient failures.
    ///
    /// `token_cost` is the estimated token count charged to the token-rate
    /// bucket for every attempt.
    pub(crate) async fn post_json(
        &self,
        url: &Url,
        body: &serde_json::Value,
        token_cost: u64,
    ) -> Result<Reply, EmbedError> {
        let payload = serde_json::to_vec(body).map_err(|e| EmbedError::Response {
            provider: self.provider,
            message: format!("cannot serialise request: {e}"),
        })?;
        let started = Instant::now();
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            match self.attempt(url, &payload, token_cost).await {
                Ok(body) => {
                    return Ok(Reply {
                        body,
                        attempts: attempt,
                        latency: started.elapsed(),
                    });
                }
                Err(Failure::Fatal(error)) => return Err(error),
                Err(Failure::Transient(transient)) => {
                    let retries_done = attempt - 1;
                    if retries_done >= self.retry.max_retries {
                        return Err(transient.into_error(self.provider, attempt));
                    }
                    let delay = match transient.retry_after() {
                        Some(wait) if wait > self.retry.max_retry_after => {
                            return Err(transient.into_error(self.provider, attempt));
                        }
                        Some(wait) => wait,
                        None => self.retry.backoff(retries_done),
                    };
                    tracing::debug!(
                        provider = self.provider,
                        attempt,
                        delay_ms = delay.as_millis() as u64,
                        "retrying embedding request after a transient failure"
                    );
                    tokio::time::sleep(delay).await;
                }
            }
        }
    }

    async fn attempt(
        &self,
        url: &Url,
        payload: &[u8],
        token_cost: u64,
    ) -> Result<Vec<u8>, Failure> {
        if let Some(bucket) = &self.requests {
            bucket.acquire(1).await;
        }
        if let Some(bucket) = &self.tokens {
            bucket.acquire(token_cost).await;
        }
        let _permit = self.permits.acquire().await.map_err(|_| {
            Failure::Fatal(EmbedError::Config("the request limiter was closed".into()))
        })?;

        let mut request = self
            .client
            .post(url.clone())
            .header(CONTENT_TYPE, "application/json")
            .body(payload.to_vec());
        if let Some((name, value)) = &self.auth {
            request = request.header(name.clone(), value.clone());
        }

        let response = match request.send().await {
            Ok(response) => response,
            Err(error) => return Err(self.classify_transport_error(error)),
        };
        let status = response.status();
        let retry_after = response
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let bytes = match response.bytes().await {
            Ok(bytes) => bytes,
            Err(error) => return Err(self.classify_transport_error(error)),
        };

        if status.is_success() {
            return Ok(bytes.to_vec());
        }
        let body = self.sanitize(&String::from_utf8_lossy(&bytes));
        match status {
            StatusCode::UNAUTHORIZED | StatusCode::FORBIDDEN => {
                Err(Failure::Fatal(EmbedError::Auth {
                    provider: self.provider,
                    status: status.as_u16(),
                }))
            }
            s if s == StatusCode::TOO_MANY_REQUESTS
                || s == StatusCode::REQUEST_TIMEOUT
                || s.is_server_error() =>
            {
                Err(Failure::Transient(Transient::Status {
                    status: s.as_u16(),
                    body,
                    retry_after,
                }))
            }
            s => Err(Failure::Fatal(EmbedError::Http {
                provider: self.provider,
                status: s.as_u16(),
                body,
            })),
        }
    }

    fn classify_transport_error(&self, error: reqwest::Error) -> Failure {
        if error.is_timeout() {
            return Failure::Transient(Transient::Timeout);
        }
        if error.is_builder() {
            return Failure::Fatal(EmbedError::Config(
                self.sanitize(&error.without_url().to_string()),
            ));
        }
        Failure::Transient(Transient::Network(
            self.sanitize(&error.without_url().to_string()),
        ))
    }
}

/// `Retry-After` as delay-seconds (integer or fractional). The HTTP-date
/// form is not supported and is treated as absent, falling back to backoff.
fn parse_retry_after(value: &str) -> Option<Duration> {
    let value = value.trim();
    if let Ok(seconds) = value.parse::<u64>() {
        return Some(Duration::from_secs(seconds));
    }
    let seconds = value.parse::<f64>().ok()?;
    if (0.0..86_400.0).contains(&seconds) {
        Some(Duration::from_secs_f64(seconds))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after("3"), Some(Duration::from_secs(3)));
        assert_eq!(parse_retry_after(" 0 "), Some(Duration::ZERO));
        assert_eq!(parse_retry_after("0.5"), Some(Duration::from_millis(500)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("-1"), None);
        assert_eq!(parse_retry_after("nan"), None);
        assert_eq!(parse_retry_after(""), None);
    }

    #[test]
    fn sanitize_masks_before_truncating() {
        let key = SecretString::from("AIzaSyFAKE-key-for-transport-test".to_owned());
        let t = Transport::new(
            "gemini",
            &RequestLimits::default(),
            Some((AuthScheme::Header("x-goog-api-key"), &key)),
        )
        .unwrap();
        // Key straddles the truncation boundary.
        let text = format!("{}{}", "a".repeat(MAX_ERROR_CHARS - 5), key.expose_secret());
        let out = t.sanitize(&text);
        assert!(!out.contains("AIzaSy"), "{out}");
        assert!(out.chars().count() < MAX_ERROR_CHARS + 30);
        let long = t.sanitize(&"x".repeat(5000));
        assert!(long.ends_with("(truncated)"));
    }

    #[test]
    fn invalid_header_characters_do_not_echo_the_key() {
        let key = SecretString::from("bad\nkey-value-should-not-leak".to_owned());
        let err = Transport::new(
            "gemini",
            &RequestLimits::default(),
            Some((AuthScheme::Header("x-goog-api-key"), &key)),
        )
        .unwrap_err()
        .to_string();
        assert!(!err.contains("should-not-leak"), "{err}");
    }
}
