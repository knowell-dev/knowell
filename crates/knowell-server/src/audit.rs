//! Audit sinks. The server records an [`AuditEvent`] for every allowed
//! state-changing action and for every denied action; events hold only ids,
//! enum codes and the request id (never tokens or other secrets).

use std::collections::VecDeque;
use std::sync::Mutex;

use knowell_auth::AuditEvent;

use crate::access::BoxFuture;

/// Destination of audit events. Recording must not fail the request, so
/// implementations handle their own errors (and must not block for long).
pub trait AuditSink: Send + Sync + 'static {
    /// Records one event.
    fn record(&self, event: &AuditEvent);

    /// Resolves once every event recorded before the call has been written
    /// (or, failing that, logged). Sinks that write synchronously need not
    /// override it. Call it before the process exits.
    fn flush(&self) -> BoxFuture<'_, ()> {
        Box::pin(std::future::ready(()))
    }
}

/// Writes each event as one JSON line at `info` level to the
/// `knowell::audit` tracing target (the default sink).
#[derive(Debug, Clone, Copy, Default)]
pub struct TracingAuditSink;

impl AuditSink for TracingAuditSink {
    fn record(&self, event: &AuditEvent) {
        match event.to_json_line() {
            Ok(line) => tracing::info!(target: "knowell::audit", "{line}"),
            Err(err) => {
                tracing::warn!(target: "knowell::audit", error = %err, "audit event could not be serialised")
            }
        }
    }
}

/// Default number of events a [`MemoryAuditSink`] keeps.
pub const DEFAULT_MEMORY_AUDIT_CAPACITY: usize = 10_000;

/// Keeps the most recent events in memory (tests, and a view of recent
/// activity); the oldest are dropped beyond the capacity.
#[derive(Debug)]
pub struct MemoryAuditSink {
    capacity: usize,
    events: Mutex<VecDeque<AuditEvent>>,
}

impl Default for MemoryAuditSink {
    fn default() -> Self {
        Self::with_capacity(DEFAULT_MEMORY_AUDIT_CAPACITY)
    }
}

impl MemoryAuditSink {
    /// An empty sink keeping [`DEFAULT_MEMORY_AUDIT_CAPACITY`] events.
    pub fn new() -> Self {
        Self::default()
    }

    /// An empty sink keeping at most `capacity` events (at least 1).
    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            capacity: capacity.max(1),
            events: Mutex::new(VecDeque::new()),
        }
    }

    /// A copy of the kept events, oldest first.
    pub fn events(&self) -> Vec<AuditEvent> {
        self.events
            .lock()
            .map(|e| e.iter().cloned().collect())
            .unwrap_or_default()
    }
}

impl AuditSink for MemoryAuditSink {
    fn record(&self, event: &AuditEvent) {
        if let Ok(mut events) = self.events.lock() {
            if events.len() >= self.capacity {
                events.pop_front();
            }
            events.push_back(event.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use knowell_auth::{Action, Decision, DecisionReason, Principal, RequestId, Resource, UserId};
    use time::OffsetDateTime;

    use super::*;

    fn event(n: u128) -> AuditEvent {
        AuditEvent {
            at: OffsetDateTime::now_utc(),
            actor: Principal::User(UserId::new(uuid::Uuid::from_u128(n))),
            action: Action::ManageIndex,
            resource: Resource::Organization,
            decision: Decision {
                allowed: true,
                reason: DecisionReason::Granted,
            },
            request_id: RequestId::new(format!("req-{n}")).unwrap(),
        }
    }

    #[test]
    fn memory_sink_keeps_the_newest_events() {
        let sink = MemoryAuditSink::with_capacity(2);
        for n in 1..=3 {
            sink.record(&event(n));
        }
        let kept: Vec<String> = sink
            .events()
            .iter()
            .map(|e| e.request_id.as_str().to_owned())
            .collect();
        assert_eq!(kept, ["req-2", "req-3"]);
        TracingAuditSink.record(&event(4));
    }
}
