//! `GET /api/v1/events`: server-sent progress events.
//!
//! Each subscriber gets the events its principal may see, a `heartbeat` at
//! start and every `Limits::sse_heartbeat`, and a `resync` when it fell
//! behind. The stream ends when the server shuts down (see `serve`) or the
//! bus closes.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Extension;
use axum::extract::State;
use axum::response::AppendHeaders;
use axum::response::sse::{Event, Sse};
use futures::Stream;
use knowell_auth::{Action, ProjectFilter};
use time::OffsetDateTime;
use tokio::sync::broadcast::Receiver;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::watch;
use tokio::time::{Interval, MissedTickBehavior};

use crate::error::ApiError;
use crate::events::{ProgressEvent, ScopedEvent};
use crate::extract::Caller;
use crate::serve::ShutdownSignal;
use crate::state::AppState;

/// `GET /api/v1/events`. `X-Accel-Buffering: no` keeps reverse proxies
/// (a hub behind nginx) from buffering the stream.
pub(super) async fn stream(
    State(state): State<AppState>,
    caller: Caller,
    shutdown: Option<Extension<ShutdownSignal>>,
) -> Result<
    (
        AppendHeaders<[(&'static str, &'static str); 1]>,
        Sse<impl Stream<Item = Result<Event, Infallible>>>,
    ),
    ApiError,
> {
    caller.require_scope(&state, Action::ReadCode)?;
    let mut heartbeat = tokio::time::interval(state.config().limits.sse_heartbeat);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Delay);
    let subscription = Subscription {
        events: state.events().subscribe(),
        visible: caller.auth.visible.clone(),
        heartbeat,
        shutdown: shutdown.map(|Extension(signal)| signal.0),
    };
    let events = Sse::new(futures::stream::unfold(
        subscription,
        |mut sub| async move { sub.next_event().await.map(|event| (Ok(event), sub)) },
    ));
    Ok((AppendHeaders([("x-accel-buffering", "no")]), events))
}

struct Subscription {
    events: Receiver<Arc<ScopedEvent>>,
    visible: ProjectFilter,
    heartbeat: Interval,
    shutdown: Option<watch::Receiver<bool>>,
}

impl Subscription {
    /// The next event to send, or `None` to end the stream.
    async fn next_event(&mut self) -> Option<Event> {
        loop {
            tokio::select! {
                () = shutdown_requested(&mut self.shutdown) => return None,
                received = self.events.recv() => match received {
                    Ok(scoped) if scoped.scope.visible_to(&self.visible) => {
                        return Some(to_sse(&scoped.event));
                    }
                    Ok(_) => continue,
                    Err(RecvError::Lagged(missed)) => {
                        return Some(to_sse(&ProgressEvent::Resync { missed }));
                    }
                    Err(RecvError::Closed) => return None,
                },
                _ = self.heartbeat.tick() => {
                    return Some(to_sse(&ProgressEvent::Heartbeat {
                        at: OffsetDateTime::now_utc(),
                    }));
                }
            }
        }
    }
}

/// Resolves once shutdown was signalled (or the signal's sender is gone);
/// never resolves without a signal.
async fn shutdown_requested(signal: &mut Option<watch::Receiver<bool>>) {
    let Some(rx) = signal else {
        return std::future::pending().await;
    };
    loop {
        if *rx.borrow_and_update() {
            return;
        }
        if rx.changed().await.is_err() {
            return;
        }
    }
}

fn to_sse(event: &ProgressEvent) -> Event {
    match serde_json::to_string(event) {
        Ok(json) => Event::default().data(json),
        Err(_) => Event::default().data("{\"type\":\"resync\",\"missed\":0}"),
    }
}
