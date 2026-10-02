//! MCP tool usage counters for `GET /api/v1/usage`.
//!
//! In-process only: counters start at zero when the engine starts and are
//! not persisted (the store has no usage table yet). Tokens returned are the
//! usual estimate of four bytes of JSON per token.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Mutex, PoisonError};

use serde_json::{Value, json};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

/// Latency samples kept per tool for percentiles.
const MAX_SAMPLES: usize = 1000;

#[derive(Debug, Default)]
struct ToolCounters {
    calls: u64,
    errors: u64,
    tokens: u64,
    latencies_ms: VecDeque<u64>,
}

#[derive(Debug, Default)]
struct AgentCounters {
    sessions: BTreeSet<String>,
    calls: u64,
    tokens: u64,
    last_seen: Option<OffsetDateTime>,
}

#[derive(Debug, Default)]
struct DayCounters {
    calls: u64,
    tokens: u64,
}

#[derive(Debug, Default)]
struct UsageState {
    tools: BTreeMap<String, ToolCounters>,
    agents: BTreeMap<String, AgentCounters>,
    days: BTreeMap<String, DayCounters>,
    last_call: Option<OffsetDateTime>,
}

/// Records tool calls and renders the panel's `UsageReport`.
#[derive(Debug, Default)]
pub(crate) struct UsageRecorder {
    state: Mutex<UsageState>,
}

/// One finished tool call.
#[derive(Debug, Clone)]
pub(crate) struct CallRecord<'a> {
    pub(crate) tool: &'a str,
    pub(crate) agent: &'a str,
    pub(crate) session: &'a str,
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

fn percentile(samples: &VecDeque<u64>, p: usize) -> u64 {
    if samples.is_empty() {
        return 0;
    }
    let mut sorted: Vec<u64> = samples.iter().copied().collect();
    sorted.sort_unstable();
    let index = (sorted.len().saturating_sub(1)).saturating_mul(p) / 100;
    sorted.get(index).copied().unwrap_or(0)
}

impl UsageRecorder {
    pub(crate) fn record(&self, call: &CallRecord<'_>) {
        let tokens = u64::try_from(call.output_bytes.div_ceil(4)).unwrap_or(u64::MAX);
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let tool = state.tools.entry(call.tool.to_owned()).or_default();
        tool.calls = tool.calls.saturating_add(1);
        if !call.ok {
            tool.errors = tool.errors.saturating_add(1);
        }
        tool.tokens = tool.tokens.saturating_add(tokens);
        if tool.latencies_ms.len() >= MAX_SAMPLES {
            tool.latencies_ms.pop_front();
        }
        tool.latencies_ms.push_back(call.latency_ms);
        let agent = state.agents.entry(call.agent.to_owned()).or_default();
        agent.sessions.insert(call.session.to_owned());
        agent.calls = agent.calls.saturating_add(1);
        agent.tokens = agent.tokens.saturating_add(tokens);
        agent.last_seen = Some(call.at);
        let day = state.days.entry(date_of(call.at)).or_default();
        day.calls = day.calls.saturating_add(1);
        day.tokens = day.tokens.saturating_add(tokens);
        state.last_call = Some(call.at);
    }

    /// When the last tool call finished.
    pub(crate) fn last_call(&self) -> Option<OffsetDateTime> {
        self.state
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .last_call
    }

    /// The panel's `UsageReport` for the last `days` days (ending `now`).
    pub(crate) fn report(&self, days: u32, now: OffsetDateTime) -> Value {
        let state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        let since = now - time::Duration::days(i64::from(days.max(1)));
        let first_day = date_of(since);
        let tools: Vec<Value> = state
            .tools
            .iter()
            .map(|(name, c)| {
                json!({
                    "tool": name,
                    "calls": c.calls,
                    "errors": c.errors,
                    "tokensReturned": c.tokens,
                    "p50Ms": percentile(&c.latencies_ms, 50),
                    "p95Ms": percentile(&c.latencies_ms, 95),
                    "spendUsdMicros": 0,
                })
            })
            .collect();
        let agents: Vec<Value> = state
            .agents
            .iter()
            .filter(|(_, a)| a.last_seen.is_some_and(|t| t >= since))
            .map(|(name, a)| {
                json!({
                    "agent": name,
                    "sessions": a.sessions.len(),
                    "calls": a.calls,
                    "tokensReturned": a.tokens,
                    "lastSeen": a.last_seen.and_then(|t| t.format(&Rfc3339).ok()),
                })
            })
            .collect();
        let daily: Vec<Value> = state
            .days
            .iter()
            .filter(|(date, _)| **date > first_day)
            .map(|(date, d)| {
                json!({
                    "date": date,
                    "calls": d.calls,
                    "tokensReturned": d.tokens,
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
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_calls_errors_and_percentiles() {
        let usage = UsageRecorder::default();
        let at = OffsetDateTime::UNIX_EPOCH + time::Duration::days(20_000);
        for (i, ok) in [true, true, false].into_iter().enumerate() {
            usage.record(&CallRecord {
                tool: "search",
                agent: "claude-code",
                session: "s1",
                ok,
                output_bytes: 400,
                latency_ms: 10 * (i as u64 + 1),
                at,
            });
        }
        let report = usage.report(7, at);
        assert_eq!(report["periodDays"], 7);
        assert_eq!(report["tools"][0]["calls"], 3);
        assert_eq!(report["tools"][0]["errors"], 1);
        assert_eq!(report["tools"][0]["tokensReturned"], 300);
        assert_eq!(report["tools"][0]["p50Ms"], 20);
        assert_eq!(report["agents"][0]["sessions"], 1);
        assert_eq!(report["daily"][0]["calls"], 3);
        assert_eq!(usage.last_call(), Some(at));
    }
}
