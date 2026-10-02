//! Token and USD budget tracking.

use std::sync::{Arc, Mutex, PoisonError};

use crate::error::EmbedError;

/// A shared spending cap for embedding calls.
///
/// Clones share the same counters, so one `Budget` can guard several
/// providers or workers. A call reserves its estimated tokens up front and
/// is refused as a whole when they do not fit: work is never silently
/// truncated or half-done because of the budget.
#[derive(Debug, Clone)]
pub struct Budget {
    inner: Arc<Inner>,
}

#[derive(Debug)]
struct Inner {
    max_tokens: Option<u64>,
    max_usd: Option<f64>,
    usd_per_million_tokens: f64,
    state: Mutex<State>,
}

#[derive(Debug, Default)]
struct State {
    spent_tokens: u64,
    reserved_tokens: u64,
}

impl Budget {
    /// Creates a budget.
    ///
    /// `max_tokens` and `max_usd` are optional ceilings (both may be set);
    /// `usd_per_million_tokens` is the input-token price used to convert
    /// tokens to USD and must be finite and not negative.
    pub fn new(
        max_tokens: Option<u64>,
        max_usd: Option<f64>,
        usd_per_million_tokens: f64,
    ) -> Result<Self, EmbedError> {
        if !usd_per_million_tokens.is_finite() || usd_per_million_tokens < 0.0 {
            return Err(EmbedError::Config(
                "price per million tokens must be a finite, non-negative number".into(),
            ));
        }
        if let Some(usd) = max_usd
            && (!usd.is_finite() || usd < 0.0)
        {
            return Err(EmbedError::Config(
                "max_usd must be a finite, non-negative number".into(),
            ));
        }
        Ok(Self {
            inner: Arc::new(Inner {
                max_tokens,
                max_usd,
                usd_per_million_tokens,
                state: Mutex::new(State::default()),
            }),
        })
    }

    /// Converts tokens to USD at the configured price.
    pub fn cost_usd(&self, tokens: u64) -> f64 {
        tokens as f64 / 1_000_000.0 * self.inner.usd_per_million_tokens
    }

    /// Tokens charged so far (completed and failed-after-sending calls).
    pub fn spent_tokens(&self) -> u64 {
        self.lock().spent_tokens
    }

    /// USD charged so far.
    pub fn spent_usd(&self) -> f64 {
        self.cost_usd(self.spent_tokens())
    }

    /// Tokens still available under the tightest ceiling, or `None` when the
    /// budget is unlimited.
    pub fn remaining_tokens(&self) -> Option<u64> {
        let used = {
            let state = self.lock();
            state.spent_tokens.saturating_add(state.reserved_tokens)
        };
        self.remaining_after(used)
    }

    /// Reserves `estimated_tokens` for one call, or refuses it.
    ///
    /// The reservation settles on drop: the actual tokens recorded with
    /// [`Reservation::add_actual`] are charged and the rest is released,
    /// whether the call succeeded, failed or was cancelled.
    pub fn reserve(&self, estimated_tokens: u64) -> Result<Reservation, EmbedError> {
        let mut state = self.lock();
        let used = state.spent_tokens.saturating_add(state.reserved_tokens);
        let wanted = used.saturating_add(estimated_tokens);
        if let Some(max) = self.inner.max_tokens
            && wanted > max
        {
            return Err(EmbedError::BudgetExceeded(format!(
                "this call needs about {estimated_tokens} tokens but only {} of the {max} token budget remain; raise the budget or embed less",
                max.saturating_sub(used)
            )));
        }
        if let Some(max_usd) = self.inner.max_usd {
            let cost = self.cost_usd(wanted);
            // Tolerance: 50_000 tokens at $0.20/M is exactly $0.01 on paper but
            // not in binary floating point.
            if cost > max_usd + 1e-9 {
                return Err(EmbedError::BudgetExceeded(format!(
                    "this call would bring spending to about ${cost:.4}, above the ${max_usd:.4} budget ({estimated_tokens} estimated tokens); raise the budget or embed less"
                )));
            }
        }
        state.reserved_tokens = state.reserved_tokens.saturating_add(estimated_tokens);
        drop(state);
        Ok(Reservation {
            budget: self.clone(),
            estimated: estimated_tokens,
            actual: 0,
        })
    }

    fn remaining_after(&self, used: u64) -> Option<u64> {
        let by_tokens = self.inner.max_tokens.map(|m| m.saturating_sub(used));
        let by_usd = self.inner.max_usd.and_then(|usd| {
            if self.inner.usd_per_million_tokens > 0.0 {
                let max = usd / self.inner.usd_per_million_tokens * 1_000_000.0;
                Some(((max + 1e-6) as u64).saturating_sub(used))
            } else {
                None
            }
        });
        match (by_tokens, by_usd) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, State> {
        self.inner
            .state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// Tokens set aside for one in-flight call; see [`Budget::reserve`].
#[derive(Debug)]
pub struct Reservation {
    budget: Budget,
    estimated: u64,
    actual: u64,
}

impl Reservation {
    /// Records tokens actually consumed by a completed request.
    pub fn add_actual(&mut self, tokens: u64) {
        self.actual = self.actual.saturating_add(tokens);
    }
}

impl Drop for Reservation {
    fn drop(&mut self) {
        let mut state = self.budget.lock();
        state.reserved_tokens = state.reserved_tokens.saturating_sub(self.estimated);
        state.spent_tokens = state.spent_tokens.saturating_add(self.actual);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn token_ceiling_refuses_whole_call() {
        let budget = Budget::new(Some(1000), None, 0.0).unwrap();
        let mut r = budget.reserve(600).unwrap();
        assert!(matches!(
            budget.reserve(500),
            Err(EmbedError::BudgetExceeded(_))
        ));
        r.add_actual(550);
        drop(r);
        assert_eq!(budget.spent_tokens(), 550);
        assert_eq!(budget.remaining_tokens(), Some(450));
        assert!(budget.reserve(450).is_ok());
    }

    #[test]
    fn failed_call_releases_unused_reservation() {
        let budget = Budget::new(Some(100), None, 0.0).unwrap();
        drop(budget.reserve(100).unwrap());
        assert_eq!(budget.spent_tokens(), 0);
        assert!(budget.reserve(100).is_ok());
    }

    #[test]
    fn usd_ceiling_uses_price_per_million() {
        // $0.20 per 1M tokens, $0.01 cap = 50_000 tokens.
        let budget = Budget::new(None, Some(0.01), 0.20).unwrap();
        assert!(budget.reserve(50_000).is_ok());
        let err = budget.reserve(50_001).unwrap_err().to_string();
        assert!(err.contains("budget"), "{err}");
        assert!((budget.cost_usd(1_000_000) - 0.20).abs() < 1e-12);
        assert_eq!(budget.remaining_tokens(), Some(50_000));
    }

    #[test]
    fn clones_share_state() {
        let a = Budget::new(Some(10), None, 0.0).unwrap();
        let b = a.clone();
        let mut r = a.reserve(10).unwrap();
        r.add_actual(10);
        drop(r);
        assert_eq!(b.spent_tokens(), 10);
        assert!(b.reserve(1).is_err());
    }

    #[test]
    fn rejects_invalid_configuration() {
        assert!(Budget::new(None, None, -1.0).is_err());
        assert!(Budget::new(None, None, f64::NAN).is_err());
        assert!(Budget::new(None, Some(-1.0), 1.0).is_err());
    }

    #[test]
    fn unlimited_budget_reports_no_remaining() {
        let budget = Budget::new(None, None, 1.0).unwrap();
        assert_eq!(budget.remaining_tokens(), None);
        assert!(budget.reserve(u64::MAX).is_ok());
    }
}
