//! [`StoreAuditSink`]: audit events written to the database's append-only
//! `audit_log` (`knowell_store::audit`).

use knowell_auth::{AuditEvent, Principal};
use knowell_core::Name;
use knowell_store::audit::{self, NewAuditEntry};
use knowell_store::{OrganizationId, PrincipalId, Store, StoreError, hierarchy};
use tokio::sync::{mpsc, oneshot};

use crate::access::BoxFuture;
use crate::audit::AuditSink;
use crate::error::ServerError;

/// Default number of events a [`StoreAuditSink`] queues before it falls
/// back to logging.
pub const DEFAULT_AUDIT_QUEUE: usize = 4096;

/// Most events written in one statement.
const BATCH: usize = 256;

enum Message {
    Event(Box<AuditEvent>),
    Flush(oneshot::Sender<()>),
}

/// Writes audit events to the `audit_log` table.
///
/// [`AuditSink::record`] must not block a request, so events go through a
/// bounded queue to a background task that writes them in batches. Nothing
/// is dropped silently: when the queue is full, or a write fails, the event
/// is logged as its JSON line at `warn` level on the `knowell::audit`
/// target instead. Call [`AuditSink::flush`] (or
/// [`crate::AppState::flush_audit`]) before the process exits so queued
/// events reach the database.
///
/// Rows carry the configured organization's id once it exists (`NULL`
/// before), the actor's text form and acting principal (an agent's user),
/// the action and resource text, the decision and the request id — never a
/// token or other secret.
#[derive(Debug, Clone)]
pub struct StoreAuditSink {
    sender: mpsc::Sender<Message>,
}

impl std::fmt::Debug for Message {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Event(_) => f.write_str("Event"),
            Self::Flush(_) => f.write_str("Flush"),
        }
    }
}

impl StoreAuditSink {
    /// Starts the writer task on the current tokio runtime, with a queue of
    /// [`DEFAULT_AUDIT_QUEUE`] events.
    ///
    /// # Errors
    /// [`ServerError::Config`] when called outside a tokio runtime.
    pub fn spawn(store: Store, organization: Name) -> Result<Self, ServerError> {
        Self::spawn_with_capacity(store, organization, DEFAULT_AUDIT_QUEUE)
    }

    /// Like [`StoreAuditSink::spawn`] with a queue of `capacity` events (at
    /// least 1).
    ///
    /// # Errors
    /// [`ServerError::Config`] when called outside a tokio runtime.
    pub fn spawn_with_capacity(
        store: Store,
        organization: Name,
        capacity: usize,
    ) -> Result<Self, ServerError> {
        let runtime = tokio::runtime::Handle::try_current().map_err(|_| {
            ServerError::Config("the database audit sink needs a running tokio runtime".to_owned())
        })?;
        let (sender, receiver) = mpsc::channel(capacity.max(1));
        runtime.spawn(write_events(store, organization, receiver));
        Ok(Self { sender })
    }
}

impl AuditSink for StoreAuditSink {
    fn record(&self, event: &AuditEvent) {
        if self
            .sender
            .try_send(Message::Event(Box::new(event.clone())))
            .is_err()
        {
            log_instead(event, "the audit queue is full or closed");
        }
    }

    fn flush(&self) -> BoxFuture<'_, ()> {
        Box::pin(async move {
            let (done, wait) = oneshot::channel();
            if self.sender.send(Message::Flush(done)).await.is_ok() {
                let _ = wait.await;
            }
        })
    }
}

/// Keeps an event that could not be stored in the log stream.
fn log_instead(event: &AuditEvent, why: &str) {
    match event.to_json_line() {
        Ok(line) => tracing::warn!(target: "knowell::audit", reason = why, "{line}"),
        Err(err) => {
            tracing::warn!(target: "knowell::audit", reason = why, error = %err, "audit event lost")
        }
    }
}

/// The background writer: batches queued events, answers flushes after the
/// events before them are written, ends when every sender is gone.
async fn write_events(store: Store, organization: Name, mut receiver: mpsc::Receiver<Message>) {
    let mut organization_id: Option<OrganizationId> = None;
    let mut events: Vec<AuditEvent> = Vec::new();
    let mut flushes: Vec<oneshot::Sender<()>> = Vec::new();
    while let Some(first) = receiver.recv().await {
        let mut next = Some(first);
        while let Some(message) = next.take() {
            match message {
                Message::Event(event) => events.push(*event),
                Message::Flush(done) => flushes.push(done),
            }
            if events.len() < BATCH {
                next = receiver.try_recv().ok();
            }
        }
        if !events.is_empty() {
            if let Err(err) =
                write_batch(&store, &organization, &mut organization_id, &events).await
            {
                tracing::warn!(target: "knowell::audit", error = %err, "audit events could not be stored");
                for event in &events {
                    log_instead(event, "the audit log write failed");
                }
            }
            events.clear();
        }
        for done in flushes.drain(..) {
            let _ = done.send(());
        }
    }
}

async fn write_batch(
    store: &Store,
    organization: &Name,
    organization_id: &mut Option<OrganizationId>,
    events: &[AuditEvent],
) -> Result<(), StoreError> {
    let mut conn = store.acquire().await?;
    if organization_id.is_none() {
        *organization_id = hierarchy::find_organization(&mut conn, organization)
            .await?
            .map(|org| org.id);
    }
    let entries: Vec<NewAuditEntry> = events
        .iter()
        .map(|event| entry(event, *organization_id))
        .collect();
    audit::append_audit(&mut conn, &entries).await?;
    Ok(())
}

/// The row for one event.
fn entry(event: &AuditEvent, organization: Option<OrganizationId>) -> NewAuditEntry {
    let principal = match &event.actor {
        Principal::User(user) => user.as_uuid(),
        Principal::ServiceAccount(account) => account.as_uuid(),
        Principal::Agent { on_behalf_of, .. } => on_behalf_of.as_uuid(),
    };
    NewAuditEntry {
        organization,
        at: event.at,
        actor: event.actor.to_string(),
        principal: Some(PrincipalId(principal)),
        action: event.action.to_string(),
        resource: event.resource.to_string(),
        allowed: event.decision.allowed,
        reason: event.decision.reason.code().to_owned(),
        request_id: event.request_id.as_str().to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use knowell_auth::{
        Action, AgentSessionId, Decision, DecisionReason, RequestId, Resource, UserId,
    };
    use time::OffsetDateTime;
    use uuid::Uuid;

    use super::*;

    #[test]
    fn entries_keep_codes_and_the_acting_user() {
        let event = AuditEvent {
            at: OffsetDateTime::UNIX_EPOCH,
            actor: Principal::Agent {
                on_behalf_of: UserId::new(Uuid::from_u128(1)),
                client: Name::new("test-agent").unwrap(),
                session: AgentSessionId::new(Uuid::from_u128(2)),
            },
            action: Action::ReadUncommittedOverlay(UserId::new(Uuid::from_u128(3))),
            resource: Resource::project(Name::new("main").unwrap(), Name::new("api").unwrap()),
            decision: Decision {
                allowed: false,
                reason: DecisionReason::DeniedOverlayPrivate,
            },
            request_id: RequestId::new("req-1").unwrap(),
        };
        let row = entry(&event, None);
        assert_eq!(row.principal, Some(PrincipalId(Uuid::from_u128(1))));
        assert!(
            row.actor
                .starts_with("agent:00000000-0000-0000-0000-000000000001:test-agent:")
        );
        assert_eq!(
            row.action,
            "read_uncommitted_overlay:00000000-0000-0000-0000-000000000003"
        );
        assert_eq!(row.resource, "project:main/api");
        assert_eq!(row.reason, "denied_overlay_private");
        assert!(!row.allowed);
    }
}
