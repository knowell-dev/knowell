//! Rate limiting, retry policy, batch limits and token estimation.

use std::sync::Mutex;
use std::sync::PoisonError;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use crate::error::EmbedError;

/// Conservative token estimate for budgeting and batching: one token per
/// three characters, rounded up, at least one. Source code averages roughly
/// 3.5 characters per token, so this overestimates slightly, which keeps
/// batches under provider limits. Providers report real usage when they can;
/// the estimate is only used for planning and as a fallback.
pub fn estimate_tokens(text: &str) -> u64 {
    let chars = text.chars().count() as u64;
    chars.div_ceil(3).max(1)
}

/// Size limits that decide how inputs are split into requests.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatchLimits {
    /// Maximum inputs per request (Gemini accepts at most 100 entries here).
    pub max_entries: usize,
    /// Maximum estimated tokens per request, summed over its entries.
    pub max_batch_tokens: u64,
    /// Maximum estimated tokens of a single input; larger inputs are
    /// refused with [`EmbedError::InputTooLong`], never truncated.
    pub max_input_tokens: u64,
}

impl BatchLimits {
    pub(crate) fn validate(&self, hard_max_entries: usize) -> Result<(), EmbedError> {
        if self.max_entries == 0 || self.max_entries > hard_max_entries {
            return Err(EmbedError::Config(format!(
                "max_entries must be between 1 and {hard_max_entries}"
            )));
        }
        if self.max_input_tokens == 0 {
            return Err(EmbedError::Config(
                "max_input_tokens must be positive".into(),
            ));
        }
        if self.max_batch_tokens < self.max_input_tokens {
            return Err(EmbedError::Config(
                "max_batch_tokens must be at least max_input_tokens".into(),
            ));
        }
        Ok(())
    }
}

/// Retry behaviour for 429, 5xx, timeouts and connection failures.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// Retries after the first attempt (0 disables retrying).
    pub max_retries: u32,
    /// Delay before the first retry; doubles per retry.
    pub base_delay: Duration,
    /// Upper bound of the computed backoff delay.
    pub max_delay: Duration,
    /// A `Retry-After` longer than this is not waited for: the call fails
    /// with [`EmbedError::RateLimited`] so the caller can reschedule.
    pub max_retry_after: Duration,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            max_retries: 4,
            base_delay: Duration::from_millis(500),
            max_delay: Duration::from_secs(30),
            max_retry_after: Duration::from_secs(120),
        }
    }
}

impl RetryPolicy {
    /// Exponential backoff with jitter for the `retry`-th retry (0-based):
    /// uniformly distributed in `[delay / 2, delay]` where
    /// `delay = min(max_delay, base_delay * 2^retry)`.
    pub(crate) fn backoff(&self, retry: u32) -> Duration {
        let factor = 1u32.checked_shl(retry.min(20)).unwrap_or(u32::MAX);
        let full = self.base_delay.saturating_mul(factor).min(self.max_delay);
        let jitter = jitter_fraction();
        full.mul_f64(0.5 + 0.5 * jitter)
    }
}

/// A pseudo-random fraction in `[0, 1)`. Jitter only needs to decorrelate
/// concurrent clients, so a clock-seeded splitmix64 step is enough and avoids
/// a random-number dependency.
fn jitter_fraction() -> f64 {
    static COUNTER: AtomicU64 = AtomicU64::new(0x9E37_79B9_7F4A_7C15);
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos() as u64)
        .unwrap_or(0);
    let step = COUNTER.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed);
    let mut z = step ^ nanos.rotate_left(32);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    (z >> 11) as f64 / (1u64 << 53) as f64
}

/// Per-provider request limits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RequestLimits {
    /// Timeout of one HTTP attempt.
    pub timeout: Duration,
    /// Maximum requests in flight at once for this provider instance.
    pub max_concurrency: usize,
    /// Provider request quota per minute (continuously refilled token bucket).
    /// Gemini conservatively charges one unit per input entry; OpenAI-compatible
    /// and Ollama providers charge one unit per HTTP batch. Each retry consumes
    /// the same quota again. `None` = unlimited.
    pub requests_per_minute: Option<u32>,
    /// Estimated-token rate cap (token bucket). `None` = unlimited. Must be
    /// at least the batch's `max_batch_tokens`.
    pub tokens_per_minute: Option<u32>,
    /// Retry policy.
    pub retry: RetryPolicy,
}

impl Default for RequestLimits {
    fn default() -> Self {
        Self {
            timeout: Duration::from_secs(60),
            max_concurrency: 4,
            requests_per_minute: None,
            tokens_per_minute: None,
            retry: RetryPolicy::default(),
        }
    }
}

impl RequestLimits {
    pub(crate) fn validate(&self, batch: &BatchLimits) -> Result<(), EmbedError> {
        if self.max_concurrency == 0 {
            return Err(EmbedError::Config(
                "max_concurrency must be at least 1".into(),
            ));
        }
        if self.timeout.is_zero() {
            return Err(EmbedError::Config("timeout must be positive".into()));
        }
        if self.requests_per_minute == Some(0) || self.tokens_per_minute == Some(0) {
            return Err(EmbedError::Config(
                "rate limits must be positive when set".into(),
            ));
        }
        if let Some(tpm) = self.tokens_per_minute
            && u64::from(tpm) < batch.max_batch_tokens
        {
            return Err(EmbedError::Config(
                "tokens_per_minute must be at least max_batch_tokens, or no batch could ever be sent"
                    .into(),
            ));
        }
        Ok(())
    }
}

/// Joint request and token quota reservation. Both costs are consumed at
/// the same instant, after both continuously refilled buckets are ready.
#[derive(Debug)]
pub(crate) struct RateLimiter {
    state: Mutex<RateState>,
}

impl RateLimiter {
    /// Builds optional buckets, each measured in quota units per minute.
    pub(crate) fn new(requests_per_minute: Option<u32>, tokens_per_minute: Option<u32>) -> Self {
        Self {
            state: Mutex::new(RateState::new(
                requests_per_minute,
                tokens_per_minute,
                Instant::now(),
            )),
        }
    }

    /// Waits until both costs are available and consumes them together.
    pub(crate) async fn acquire(&self, request_cost: u64, token_cost: u64) {
        loop {
            let wait = self
                .state
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .try_acquire(request_cost, token_cost, Instant::now());
            match wait {
                None => return,
                Some(wait) => tokio::time::sleep(wait).await,
            }
        }
    }
}

#[derive(Debug)]
struct RateState {
    requests: Option<TokenBucket>,
    tokens: Option<TokenBucket>,
    updated_at: Instant,
}

impl RateState {
    fn new(requests_per_minute: Option<u32>, tokens_per_minute: Option<u32>, now: Instant) -> Self {
        Self {
            requests: requests_per_minute.map(TokenBucket::per_minute),
            tokens: tokens_per_minute.map(TokenBucket::per_minute),
            updated_at: now,
        }
    }

    fn try_acquire(
        &mut self,
        request_cost: u64,
        token_cost: u64,
        now: Instant,
    ) -> Option<Duration> {
        let elapsed = now.saturating_duration_since(self.updated_at).as_secs_f64();
        self.updated_at = now;
        if let Some(bucket) = &mut self.requests {
            bucket.refill(elapsed);
        }
        if let Some(bucket) = &mut self.tokens {
            bucket.refill(elapsed);
        }
        let request_wait = self
            .requests
            .as_ref()
            .map_or(0.0, |bucket| bucket.wait_seconds(request_cost));
        let token_wait = self
            .tokens
            .as_ref()
            .map_or(0.0, |bucket| bucket.wait_seconds(token_cost));
        let wait = request_wait.max(token_wait);
        if wait > 0.0 {
            return Some(Duration::from_secs_f64(wait.clamp(0.001, 3600.0)));
        }
        // Reserving one quota before waiting for the other would let that
        // reservation age and refill, causing bursts when the wait ends.
        if let Some(bucket) = &mut self.requests {
            bucket.consume(request_cost);
        }
        if let Some(bucket) = &mut self.tokens {
            bucket.consume(token_cost);
        }
        None
    }
}

/// One continuously refilled bucket, accessed only under the joint mutex.
/// Oversized costs retain the existing clamp to capacity; provider config
/// validation ensures valid batches fit both configured quota capacities.
#[derive(Debug)]
struct TokenBucket {
    capacity: f64,
    per_second: f64,
    available: f64,
}

impl TokenBucket {
    /// A bucket holding `per_minute` units, refilled at `per_minute / 60` per second.
    fn per_minute(per_minute: u32) -> Self {
        let capacity = f64::from(per_minute.max(1));
        Self {
            capacity,
            per_second: capacity / 60.0,
            available: capacity,
        }
    }

    fn refill(&mut self, elapsed: f64) {
        self.available = (self.available + elapsed * self.per_second).min(self.capacity);
    }

    fn cost(&self, cost: u64) -> f64 {
        (cost as f64).clamp(0.0, self.capacity)
    }

    fn wait_seconds(&self, cost: u64) -> f64 {
        ((self.cost(cost) - self.available) / self.per_second).max(0.0)
    }

    fn consume(&mut self, cost: u64) {
        self.available -= self.cost(cost);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_estimate_is_conservative_and_never_zero() {
        assert_eq!(estimate_tokens(""), 1);
        assert_eq!(estimate_tokens("abc"), 1);
        assert_eq!(estimate_tokens("abcd"), 2);
        assert_eq!(estimate_tokens(&"x".repeat(300)), 100);
        // Counts characters, not bytes.
        assert_eq!(estimate_tokens("ççç"), 1);
    }

    #[test]
    fn backoff_grows_and_is_capped() {
        let policy = RetryPolicy {
            max_retries: 10,
            base_delay: Duration::from_millis(100),
            max_delay: Duration::from_secs(1),
            max_retry_after: Duration::from_secs(5),
        };
        for _ in 0..50 {
            let first = policy.backoff(0);
            assert!(first >= Duration::from_millis(50) && first <= Duration::from_millis(100));
            let third = policy.backoff(2);
            assert!(third >= Duration::from_millis(200) && third <= Duration::from_millis(400));
            let huge = policy.backoff(60);
            assert!(huge >= Duration::from_millis(500) && huge <= Duration::from_secs(1));
        }
    }

    #[test]
    fn jitter_stays_in_unit_interval() {
        for _ in 0..1000 {
            let j = jitter_fraction();
            assert!((0.0..1.0).contains(&j));
        }
    }

    #[test]
    fn limits_validation() {
        let batch = BatchLimits {
            max_entries: 10,
            max_batch_tokens: 1000,
            max_input_tokens: 500,
        };
        assert!(batch.validate(100).is_ok());
        assert!(batch.validate(5).is_err());
        let bad = BatchLimits {
            max_batch_tokens: 100,
            ..batch
        };
        assert!(bad.validate(100).is_err());
        let limits = RequestLimits {
            tokens_per_minute: Some(999),
            ..RequestLimits::default()
        };
        assert!(limits.validate(&batch).is_err());
        let limits = RequestLimits {
            max_concurrency: 0,
            ..RequestLimits::default()
        };
        assert!(limits.validate(&batch).is_err());
        assert!(RequestLimits::default().validate(&batch).is_ok());
    }

    #[tokio::test]
    async fn bucket_serves_burst_then_throttles() {
        // 6000/min = 100/s: the first 6000 units are free, the next wait.
        let bucket = RateLimiter::new(Some(6000), None);
        let start = Instant::now();
        bucket.acquire(6000, 0).await;
        assert!(start.elapsed() < Duration::from_millis(100));
        bucket.acquire(20, 0).await; // needs ~0.2 s of refill
        assert!(start.elapsed() >= Duration::from_millis(150));
    }

    #[tokio::test]
    async fn oversized_cost_is_clamped_not_deadlocked() {
        let bucket = RateLimiter::new(Some(60), None);
        bucket.acquire(1_000_000, 0).await;
    }

    #[test]
    fn joint_wait_consumes_request_quota_only_when_tokens_are_ready() {
        let start = Instant::now();
        let mut state = RateState::new(Some(60), Some(120), start);
        assert_eq!(state.try_acquire(60, 120, start), None);
        assert_eq!(
            state.try_acquire(10, 120, start + Duration::from_secs(30)),
            Some(Duration::from_secs(30))
        );
        let ready = start + Duration::from_secs(60);
        assert_eq!(state.try_acquire(10, 120, ready), None);
        // The ten request units must be charged now, rather than before
        // the token wait, or another full request burst could go out now.
        assert_eq!(
            state.try_acquire(60, 0, ready),
            Some(Duration::from_secs(10))
        );
    }

    #[test]
    fn waiting_for_one_quota_does_not_reserve_the_other() {
        let start = Instant::now();
        let mut state = RateState::new(Some(60), Some(120), start);
        assert_eq!(state.try_acquire(60, 120, start), None);
        let halfway = start + Duration::from_secs(30);
        assert_eq!(
            state.try_acquire(10, 120, halfway),
            Some(Duration::from_secs(30))
        );
        // A token-blocked caller must leave the available request quota
        // untouched so a smaller call can still proceed.
        assert_eq!(state.try_acquire(30, 60, halfway), None);

        let mut state = RateState::new(Some(60), Some(120), start);
        assert_eq!(state.try_acquire(60, 0, start), None);
        assert_eq!(
            state.try_acquire(60, 60, start),
            Some(Duration::from_secs(60))
        );
        // Symmetrically, a request-blocked caller cannot reserve tokens.
        assert_eq!(
            state.try_acquire(1, 120, start + Duration::from_secs(1)),
            None
        );
    }
}
