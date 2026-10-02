//! Progress events for `GET /api/v1/events` (server-sent events).
//!
//! The indexer (and the server itself, for jobs it enqueues) publishes
//! [`ProgressEvent`]s on an [`EventBus`]; every SSE subscriber receives the
//! events whose [`EventScope`] it may see. Delivery is best effort: a
//! subscriber that falls behind by more than the bus capacity receives one
//! `resync` event telling it how many it missed, and should refetch state.

use std::sync::Arc;

use knowell_auth::ProjectFilter;
use knowell_core::Name;
use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::broadcast;

use crate::wire::{GenerationView, JobView};

/// Default number of events buffered per subscriber.
pub const DEFAULT_EVENT_CAPACITY: usize = 1024;

/// One progress event, serialised as `{"type": "...", ...}`.
#[derive(Debug, Clone, PartialEq, Serialize)]
#[serde(
    tag = "type",
    rename_all = "kebab-case",
    rename_all_fields = "camelCase"
)]
pub enum ProgressEvent {
    /// A job changed state.
    Job {
        /// The job.
        job: JobView,
    },
    /// A view generation changed state or progressed.
    Generation {
        /// View store id.
        view_id: String,
        /// The generation.
        generation: GenerationView,
    },
    /// Keep-alive sent by the server at a fixed interval.
    Heartbeat {
        /// Server time.
        #[serde(with = "time::serde::rfc3339")]
        at: OffsetDateTime,
    },
    /// The subscriber fell behind and missed events; refetch state.
    Resync {
        /// Number of events dropped for this subscriber.
        missed: u64,
    },
}

/// Who may see an event.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EventScope {
    /// Organization-wide data (only principals that see every project).
    Organization,
    /// One project.
    Project {
        /// Workspace name.
        workspace: Name,
        /// Project name.
        project: Name,
    },
}

impl EventScope {
    /// Whether a subscriber with `visible` may receive the event.
    pub fn visible_to(&self, visible: &ProjectFilter) -> bool {
        match self {
            Self::Organization => visible.is_all(),
            Self::Project { workspace, project } => visible.allows(workspace, project),
        }
    }
}

/// An event with its visibility scope.
#[derive(Debug, Clone, PartialEq)]
pub struct ScopedEvent {
    /// Who may see it.
    pub scope: EventScope,
    /// The event.
    pub event: ProgressEvent,
}

/// Broadcast channel of progress events. Cloning shares the channel.
#[derive(Debug, Clone)]
pub struct EventBus {
    sender: broadcast::Sender<Arc<ScopedEvent>>,
}

impl EventBus {
    /// A bus buffering up to `capacity` events per subscriber (at least 1).
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity.max(1));
        Self { sender }
    }

    /// Publishes an event; returns how many subscribers it was queued for
    /// (0 when nobody listens, which is not an error).
    pub fn publish(&self, scope: EventScope, event: ProgressEvent) -> usize {
        self.sender
            .send(Arc::new(ScopedEvent { scope, event }))
            .unwrap_or(0)
    }

    /// A new subscription receiving events published from now on.
    pub fn subscribe(&self) -> broadcast::Receiver<Arc<ScopedEvent>> {
        self.sender.subscribe()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(DEFAULT_EVENT_CAPACITY)
    }
}

#[cfg(test)]
mod tests {
    use knowell_auth::{Grant, GrantSet, Principal, ResourceScope, Role, UserId, visible_projects};
    use uuid::Uuid;

    use super::*;

    fn n(s: &str) -> Name {
        Name::new(s).unwrap()
    }

    #[test]
    fn events_serialise_like_the_panel_expects() {
        let event = ProgressEvent::Resync { missed: 3 };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"type": "resync", "missed": 3})
        );
        let at = OffsetDateTime::from_unix_timestamp(0).unwrap();
        let event = ProgressEvent::Heartbeat { at };
        assert_eq!(
            serde_json::to_value(&event).unwrap(),
            serde_json::json!({"type": "heartbeat", "at": "1970-01-01T00:00:00Z"})
        );
    }

    #[test]
    fn scope_visibility() {
        let user = Principal::User(UserId::new(Uuid::from_u128(1)));
        let mut grants = GrantSet::new();
        grants.add(
            Grant::new(
                user.clone(),
                Role::Viewer,
                ResourceScope::project(n("w"), n("p")),
            )
            .unwrap(),
        );
        let visible = visible_projects(&user, &grants);
        let own = EventScope::Project {
            workspace: n("w"),
            project: n("p"),
        };
        let other = EventScope::Project {
            workspace: n("w"),
            project: n("q"),
        };
        assert!(own.visible_to(&visible));
        assert!(!other.visible_to(&visible));
        assert!(!EventScope::Organization.visible_to(&visible));
    }

    #[tokio::test]
    async fn publish_without_subscribers_is_fine() {
        let bus = EventBus::new(4);
        assert_eq!(
            bus.publish(
                EventScope::Organization,
                ProgressEvent::Resync { missed: 0 }
            ),
            0
        );
        let mut rx = bus.subscribe();
        assert_eq!(
            bus.publish(
                EventScope::Organization,
                ProgressEvent::Resync { missed: 1 }
            ),
            1
        );
        let got = rx.recv().await.unwrap();
        assert_eq!(got.event, ProgressEvent::Resync { missed: 1 });
    }
}
