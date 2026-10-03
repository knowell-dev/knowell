//! MCP tool usage for `GET /api/v1/usage` and the integration status.
//!
//! Calls are buffered in memory per UTC hour, tool and agent label, and added
//! to the store's `tool_usage_hour` rows by [`crate::Engine::flush_usage`]:
//! every few seconds from a background task and on demand. Reports combine
//! the stored hours with what is still buffered, so they survive restarts and
//! include the newest calls; a crash loses at most the unflushed buffer.
//!
//! Tokens returned are the usual estimate of four bytes of JSON per token.
//! Latency percentiles are the bound of their histogram bucket
//! ([`knowell_store::usage::latency_bucket_bound_ms`]), at most 25 % above the
//! true value. Sessions are not told apart yet: each agent label counts as one.

use std::collections::BTreeMap;
use std::sync::{Mutex, PoisonError};

use knowell_store::usage::{
    LATENCY_BUCKETS, ToolUsage, agent_label, hour_of, latency_bucket, latency_bucket_bound_ms,
};
use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

type Key = (OffsetDateTime, String, String);

#[derive(Debug, Default)]
struct Pending {
    rows: BTreeMap<Key, ToolUsage>,
    last_call: Option<OffsetDateTime>,
    last_prune: Option<OffsetDateTime>,
}

/// Buffers tool calls until they are flushed to the store.
#[derive(Debug, Default)]
pub(crate) struct UsageRecorder {
    pending: Mutex<Pending>,
}

/// One finished tool call.
#[derive(Debug, Clone)]
pub(crate) struct CallRecord<'a> {
    pub(crate) tool: &'a str,
    /// The client's self-reported name (untrusted; sanitized when stored).
    pub(crate) agent: &'a str,
    pub(crate) ok: bool,
    pub(crate) output_bytes: usize,
    pub(crate) latency_ms: u64,
    pub(crate) at: OffsetDateTime,
}

fn date_of(at: OffsetDateTime) -> String {
    let date = at.date();
    format!(
        "{:04}-{:02}-{:02}",
        date.year(),
        u8::from(date.month()),
        date.day()
    )
}

/// Adds `from` into `into` (same hour, tool and agent).
fn merge(into: &mut ToolUsage, from: &ToolUsage) {
    into.calls = into.calls.saturating_add(from.calls);
    into.errors = into.errors.saturating_add(from.errors);
    into.tokens_returned = into.tokens_returned.saturating_add(from.tokens_returned);
    into.latency_ms_sum = into.latency_ms_sum.saturating_add(from.latency_ms_sum);
    for (sum, add) in into.latency_buckets.iter_mut().zip(&from.latency_buckets) {
        *sum = sum.saturating_add(*add);
    }
    into.last_call_at = into.last_call_at.max(from.last_call_at);
}

impl Pending {
    fn add(&mut self, row: ToolUsage) {
        let key = (row.hour, row.tool.clone(), row.agent.clone());
        match self.rows.get_mut(&key) {
            Some(existing) => merge(existing, &row),
            None => {
                self.rows.insert(key, row);
            }
        }
    }
}

impl UsageRecorder {
    pub(crate) fn record(&self, call: &CallRecord<'_>) {
        // The store keeps microseconds; buffered and stored reports agree.
        let at = call
            .at
            .replace_nanosecond(call.at.nanosecond() / 1000 * 1000)
            .unwrap_or(call.at);
        let Ok(hour) = hour_of(at) else {
            tracing::warn!("a tool call outside the representable time range was not counted");
            return;
        };
        let mut latency_buckets = vec![0; LATENCY_BUCKETS];
        if let Some(bucket) = latency_buckets.get_mut(latency_bucket(call.latency_ms)) {
            *bucket = 1;
        }
        let row = ToolUsage {
            hour,
            tool: call.tool.to_owned(),
            agent: agent_label(call.agent),
            calls: 1,
            errors: u64::from(!call.ok),
            tokens_returned: u64::try_from(call.output_bytes.div_ceil(4)).unwrap_or(u64::MAX),
            latency_ms_sum: call.latency_ms,
            latency_buckets,
            last_call_at: at,
        };
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.last_call = pending.last_call.max(Some(at));
        pending.add(row);
    }

    /// Removes and returns the buffered rows (to flush them).
    pub(crate) fn take(&self) -> Vec<ToolUsage> {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        std::mem::take(&mut pending.rows).into_values().collect()
    }

    /// Puts rows back whose flush failed; they are retried with the next one.
    pub(crate) fn restore(&self, rows: Vec<ToolUsage>) {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        for row in rows {
            pending.add(row);
        }
    }

    /// A copy of the buffered rows.
    pub(crate) fn buffered(&self) -> Vec<ToolUsage> {
        let pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        pending.rows.values().cloned().collect()
    }

    /// When the newest call recorded by this process finished.
    pub(crate) fn last_call(&self) -> Option<OffsetDateTime> {
        self.pending
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_call
    }

    /// Whether old hours should be pruned now (at most once a day per
    /// process); claims the prune when it is due.
    pub(crate) fn prune_due(&self, now: OffsetDateTime) -> bool {
        let mut pending = self.pending.lock().unwrap_or_else(PoisonError::into_inner);
        let due = pending
            .last_prune
            .is_none_or(|last| now - last >= time::Duration::days(1));
        if due {
            pending.last_prune = Some(now);
        }
        due
    }
}

/// The percentile `p` (0 to 100) of a latency histogram: the bound of the
/// bucket holding that rank, 0 without calls.
fn percentile(buckets: &[u64], p: u64) -> u64 {
    let total: u64 = buckets.iter().fold(0u64, |sum, n| sum.saturating_add(*n));
    if total == 0 {
        return 0;
    }
    // Zero-based rank, as for a sorted list of every sample.
    let rank = total.saturating_sub(1).saturating_mul(p) / 100;
    let mut seen = 0u64;
    for (index, count) in buckets.iter().enumerate() {
        seen = seen.saturating_add(*count);
        if seen > rank {
            return latency_bucket_bound_ms(index).unwrap_or(0);
        }
    }
    0
}

/// The panel's `UsageReport` for the last `days` days ending `now`, from the
/// stored hourly `rows` merged with the `buffered` rows not yet flushed.
pub(crate) fn report(
    rows: Vec<ToolUsage>,
    buffered: Vec<ToolUsage>,
    days: u32,
    now: OffsetDateTime,
) -> Value {
    let since = now - time::Duration::days(i64::from(days.max(1)));
    let first_hour = hour_of(since).unwrap_or(since);
    let mut merged = Pending::default();
    for row in rows.into_iter().chain(buffered) {
        if row.hour >= first_hour {
            merged.add(row);
        }
    }
    let mut tools: BTreeMap<String, ToolUsage> = BTreeMap::new();
    let mut agents: BTreeMap<String, ToolUsage> = BTreeMap::new();
    let mut daily: BTreeMap<String, ToolUsage> = BTreeMap::new();
    for row in merged.rows.into_values() {
        for (map, key) in [
            (&mut tools, row.tool.clone()),
            (&mut agents, row.agent.clone()),
            (&mut daily, date_of(row.hour)),
        ] {
            match map.get_mut(&key) {
                Some(sum) => merge(sum, &row),
                None => {
                    map.insert(key, row.clone());
                }
            }
        }
    }
    let tools: Vec<Value> = tools
        .iter()
        .map(|(name, u)| {
            json!({
                "tool": name,
                "calls": u.calls,
                "errors": u.errors,
                "tokensReturned": u.tokens_returned,
                "p50Ms": percentile(&u.latency_buckets, 50),
                "p95Ms": percentile(&u.latency_buckets, 95),
                "spendUsdMicros": 0,
            })
        })
        .collect();
    let agents: Vec<Value> = agents
        .iter()
        .map(|(name, u)| {
            json!({
                "agent": name,
                "sessions": 1,
                "calls": u.calls,
                "tokensReturned": u.tokens_returned,
                "lastSeen": u.last_call_at.format(&Rfc3339).ok(),
            })
        })
        .collect();
    let daily: Vec<Value> = daily
        .iter()
        .map(|(date, u)| {
            json!({
                "date": date,
                "calls": u.calls,
                "tokensReturned": u.tokens_returned,
                "spendUsdMicros": 0,
            })
        })
        .collect();
    json!({
        "periodDays": days,
        "tools": tools,
        "agents": agents,
        "daily": daily,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(
        tool: &'static str,
        ok: bool,
        latency_ms: u64,
        at: OffsetDateTime,
    ) -> CallRecord<'static> {
        CallRecord {
            tool,
            agent: "claude-code",
            ok,
            output_bytes: 400,
            latency_ms,
            at,
        }
    }

    #[test]
    fn counts_calls_errors_and_percentiles() {
        let usage = UsageRecorder::default();
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        for (i, ok) in [true, true, false].into_iter().enumerate() {
            usage.record(&call("search", ok, 10 * (i as u64 + 1), at));
        }
        let report = report(Vec::new(), usage.buffered(), 7, at);
        assert_eq!(report["periodDays"], 7);
        assert_eq!(report["tools"][0]["calls"], 3);
        assert_eq!(report["tools"][0]["errors"], 1);
        assert_eq!(report["tools"][0]["tokensReturned"], 300);
        // The median call took 20 ms; its histogram bucket spans 20-23 ms.
        // With three calls the 95th percentile is the same second call.
        assert_eq!(report["tools"][0]["p50Ms"], 23);
        assert_eq!(report["tools"][0]["p95Ms"], 23);
        assert_eq!(report["agents"][0]["sessions"], 1);
        assert_eq!(report["daily"][0]["calls"], 3);
    }

    #[test]
    fn stored_and_buffered_hours_merge_within_the_period() {
        let usage = UsageRecorder::default();
        let now = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        let old = now - time::Duration::days(10);
        usage.record(&call("search", true, 5, old));
        usage.record(&call("search", true, 5, now));
        let stored = usage.take();
        assert!(usage.buffered().is_empty());
        usage.record(&call("search", false, 7, now));
        usage.record(&call("read_file", true, 1, now));
        let report = report(stored, usage.buffered(), 7, now);
        // The ten-day-old call is outside the seven-day period, for tools too.
        assert_eq!(report["tools"][0]["tool"], "read_file");
        assert_eq!(report["tools"][1]["calls"], 2);
        assert_eq!(report["tools"][1]["errors"], 1);
        assert_eq!(report["agents"][0]["calls"], 3);
        assert_eq!(report["daily"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn failed_flushes_are_retried() {
        let usage = UsageRecorder::default();
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        usage.record(&call("search", true, 5, at));
        let taken = usage.take();
        usage.record(&call("search", true, 5, at));
        usage.restore(taken);
        let rows = usage.take();
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].calls, 2);
        assert_eq!(usage.last_call(), Some(at));
    }
}
