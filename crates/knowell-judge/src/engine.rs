//! The HTTP engine behind the hosted providers: timeouts, retries with
//! backoff, concurrency limit, secret masking and usage counting.

use std::time::Duration;

use knowell_secrets::Masker;
use reqwest::StatusCode;
use reqwest::header::RETRY_AFTER;
use secrecy::{ExposeSecret, SecretString};
use tokio::sync::Semaphore;
use url::Url;

use crate::common::HttpOptions;
use crate::types::Counters;
use crate::{JudgeError, Usage};

/// Longest accepted response body. A rerank answer is a few kilobytes.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
/// Longest provider message copied into an error, in characters.
const MAX_MESSAGE_CHARS: usize = 300;

pub(crate) struct Engine {
    client: reqwest::Client,
    semaphore: Semaphore,
    opts: HttpOptions,
    masker: Masker,
    counters: Counters,
}

/// Outcome of one HTTP attempt.
enum Attempt {
    Done(Vec<u8>),
    Retry {
        retry_after: Option<Duration>,
        err: JudgeError,
    },
    Fail(JudgeError),
}

impl Engine {
    /// `key` is registered with the masker so it can never appear in an error.
    pub(crate) fn new(opts: &HttpOptions, key: Option<&SecretString>) -> Result<Self, JudgeError> {
        opts.validate()?;
        let client = reqwest::Client::builder()
            .timeout(opts.timeout)
            .connect_timeout(opts.connect_timeout)
            .build()
            .map_err(|e| JudgeError::InvalidConfig(format!("http client: {}", e.without_url())))?;
        let mut masker = Masker::new();
        if let Some(k) = key {
            masker.add(SecretString::from(k.expose_secret().to_owned()));
        }
        Ok(Self {
            client,
            semaphore: Semaphore::new(opts.max_concurrency),
            opts: opts.clone(),
            masker,
            counters: Counters::default(),
        })
    }

    pub(crate) fn usage(&self) -> Usage {
        self.counters.snapshot()
    }

    pub(crate) fn counters(&self) -> &Counters {
        &self.counters
    }

    /// POSTs `body` as JSON and returns the raw 2xx response body.
    ///
    /// Retries HTTP 429 and 5xx up to `max_retries` times with exponential
    /// backoff, waiting at least `Retry-After` when the server sent one.
    pub(crate) async fn post_json(
        &self,
        url: &Url,
        key: Option<&SecretString>,
        body: &serde_json::Value,
        documents: usize,
    ) -> Result<Vec<u8>, JudgeError> {
        let _permit = self
            .semaphore
            .acquire()
            .await
            .map_err(|_| JudgeError::Transport("concurrency limiter closed".into()))?;
        let mut attempt: u32 = 0;
        loop {
            self.counters.add_request(documents);
            match self.send_once(url, key, body).await {
                Attempt::Done(bytes) => return Ok(bytes),
                Attempt::Fail(err) => return Err(err),
                Attempt::Retry { retry_after, err } => {
                    if attempt >= self.opts.max_retries {
                        return Err(err);
                    }
                    let backoff = self.backoff(attempt);
                    let wait = match retry_after {
                        Some(ra) if ra > self.opts.max_retry_after => return Err(err),
                        Some(ra) => ra.max(backoff),
                        None => backoff,
                    };
                    tracing::debug!(
                        attempt,
                        wait_ms = wait.as_millis() as u64,
                        "retrying rerank request"
                    );
                    tokio::time::sleep(wait).await;
                    attempt = attempt.saturating_add(1);
                }
            }
        }
    }

    fn backoff(&self, attempt: u32) -> Duration {
        let factor = 1u32.checked_shl(attempt).unwrap_or(u32::MAX);
        self.opts
            .initial_backoff
            .saturating_mul(factor)
            .min(self.opts.max_backoff)
    }

    async fn send_once(
        &self,
        url: &Url,
        key: Option<&SecretString>,
        body: &serde_json::Value,
    ) -> Attempt {
        let mut req = self.client.post(url.clone()).json(body);
        if let Some(k) = key {
            // `bearer_auth` marks the header sensitive, so it is not logged.
            req = req.bearer_auth(k.expose_secret());
        }
        let resp = match req.send().await {
            Ok(r) => r,
            Err(e) => return Attempt::Fail(self.map_transport(e)),
        };
        let status = resp.status();
        let retry_after = resp
            .headers()
            .get(RETRY_AFTER)
            .and_then(|v| v.to_str().ok())
            .and_then(parse_retry_after);
        let bytes = match resp.bytes().await {
            Ok(b) => b,
            Err(e) => return Attempt::Fail(self.map_transport(e)),
        };
        if bytes.len() > MAX_BODY_BYTES {
            return Attempt::Fail(JudgeError::InvalidResponse(
                "response body too large".into(),
            ));
        }
        if status.is_success() {
            return Attempt::Done(bytes.to_vec());
        }
        let message = self.error_message(&bytes);
        if status == StatusCode::TOO_MANY_REQUESTS {
            return Attempt::Retry {
                retry_after,
                err: JudgeError::RateLimited {
                    retry_after_secs: retry_after.map(|d| d.as_secs()),
                },
            };
        }
        let code = status.as_u16();
        if status.is_server_error() {
            return Attempt::Retry {
                retry_after,
                err: JudgeError::Http {
                    status: code,
                    message,
                },
            };
        }
        if status == StatusCode::UNAUTHORIZED || status == StatusCode::FORBIDDEN {
            return Attempt::Fail(JudgeError::Unauthorized {
                status: code,
                message,
            });
        }
        Attempt::Fail(JudgeError::Http {
            status: code,
            message,
        })
    }

    fn map_transport(&self, e: reqwest::Error) -> JudgeError {
        if e.is_timeout() {
            return JudgeError::Timeout;
        }
        // Strip the URL: it is not secret by construction, but there is no
        // reason to carry it either.
        let text = e.without_url().to_string();
        JudgeError::Transport(self.sanitize(&text))
    }

    /// Masks known secrets, then truncates (in that order, so a cut can never
    /// leave a partial key behind).
    pub(crate) fn sanitize(&self, text: &str) -> String {
        let masked = self.masker.mask(text);
        masked.chars().take(MAX_MESSAGE_CHARS).collect()
    }

    /// Extracts a human message from a provider error body (`error`,
    /// `message` or `detail`), falling back to the raw text.
    fn error_message(&self, body: &[u8]) -> String {
        let raw = String::from_utf8_lossy(body);
        let extracted = serde_json::from_slice::<serde_json::Value>(body)
            .ok()
            .and_then(|v| pick_message(&v));
        let text = extracted.unwrap_or_else(|| raw.into_owned());
        self.sanitize(text.trim())
    }
}

fn pick_message(v: &serde_json::Value) -> Option<String> {
    for field in ["error", "message", "detail"] {
        match v.get(field) {
            Some(serde_json::Value::String(s)) => return Some(s.clone()),
            Some(inner @ serde_json::Value::Object(_)) => {
                if let Some(m) = inner.get("message").and_then(|m| m.as_str()) {
                    return Some(m.to_owned());
                }
            }
            _ => {}
        }
    }
    None
}

/// Parses the delta-seconds form of `Retry-After`. The HTTP-date form is not
/// supported and is treated as absent (normal backoff applies).
fn parse_retry_after(value: &str) -> Option<Duration> {
    value.trim().parse::<u64>().ok().map(Duration::from_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn retry_after_parsing() {
        assert_eq!(parse_retry_after(" 7 "), Some(Duration::from_secs(7)));
        assert_eq!(parse_retry_after("Wed, 21 Oct 2026 07:28:00 GMT"), None);
        assert_eq!(parse_retry_after("-1"), None);
    }

    #[test]
    fn backoff_doubles_and_caps() {
        let opts = HttpOptions {
            initial_backoff: Duration::from_millis(100),
            max_backoff: Duration::from_millis(350),
            ..HttpOptions::default()
        };
        let e = Engine::new(&opts, None).unwrap();
        assert_eq!(e.backoff(0), Duration::from_millis(100));
        assert_eq!(e.backoff(1), Duration::from_millis(200));
        assert_eq!(e.backoff(2), Duration::from_millis(350));
        assert_eq!(e.backoff(60), Duration::from_millis(350));
    }

    #[test]
    fn error_message_masks_and_truncates() {
        let key = SecretString::from("KNOWELL_CANARY_judge_key_0123456789".to_owned());
        let e = Engine::new(&HttpOptions::default(), Some(&key)).unwrap();
        let body = br#"{"detail":"bad key KNOWELL_CANARY_judge_key_0123456789 sorry"}"#;
        let m = e.error_message(body);
        assert!(!m.contains("KNOWELL_CANARY"));
        assert!(m.contains("[REDACTED]"));
        let long = "x".repeat(5000);
        assert_eq!(
            e.error_message(long.as_bytes()).chars().count(),
            MAX_MESSAGE_CHARS
        );
    }

    #[test]
    fn nested_error_message_is_found() {
        let v: serde_json::Value = serde_json::json!({"error": {"message": "nope"}});
        assert_eq!(pick_message(&v).as_deref(), Some("nope"));
    }
}
