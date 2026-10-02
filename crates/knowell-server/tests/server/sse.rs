//! The progress stream: heartbeats, visibility filtering and lag handling.

use std::time::Duration;

use axum::http::StatusCode;
use knowell_auth::{ResourceScope, Role, TokenScope};
use knowell_server::{EventBus, EventScope, ProgressEvent};
use tower::ServiceExt;

use crate::common::*;

const WAIT: Duration = Duration::from_secs(5);

async fn open(h: &Harness, token: &str) -> SseReader {
    let response = h
        .router
        .clone()
        .oneshot(get("/api/v1/events").bearer(token).build())
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(response.headers()["content-type"], "text/event-stream");
    assert_eq!(response.headers()["x-accel-buffering"], "no");
    SseReader::new(response.into_body())
}

fn resync(missed: u64) -> ProgressEvent {
    ProgressEvent::Resync { missed }
}

#[tokio::test]
async fn stream_starts_with_a_heartbeat_and_delivers_events() {
    let h = harness();
    let token = h.token(user(1), &[TokenScope::Read]);
    let mut reader = open(&h, &token).await;
    let first = reader.next(WAIT).await.unwrap();
    assert_eq!(first["type"], "heartbeat");
    assert!(first["at"].is_string());
    let queued = h
        .state
        .events()
        .publish(EventScope::Organization, resync(7));
    assert_eq!(queued, 1);
    let event = reader.next(WAIT).await.unwrap();
    assert_eq!(event, serde_json::json!({"type": "resync", "missed": 7}));
}

#[tokio::test]
async fn heartbeats_repeat() {
    let mut cfg = config();
    cfg.limits.sse_heartbeat = Duration::from_millis(30);
    let h = harness_with(cfg, |b| b);
    let token = h.token(user(1), &[TokenScope::Read]);
    let mut reader = open(&h, &token).await;
    for _ in 0..3 {
        assert_eq!(reader.next(WAIT).await.unwrap()["type"], "heartbeat");
    }
}

#[tokio::test]
async fn events_are_filtered_by_visibility() {
    let h = harness();
    h.grant(
        user(2),
        Role::Viewer,
        ResourceScope::project(n("main"), n("api")),
    );
    let token = h.token(user(2), &[TokenScope::Read]);
    let mut reader = open(&h, &token).await;
    assert_eq!(reader.next(WAIT).await.unwrap()["type"], "heartbeat");
    let bus = h.state.events();
    bus.publish(EventScope::Organization, resync(1));
    bus.publish(
        EventScope::Project {
            workspace: n("main"),
            project: n("web"),
        },
        resync(2),
    );
    bus.publish(
        EventScope::Project {
            workspace: n("main"),
            project: n("api"),
        },
        resync(3),
    );
    let event = reader.next(WAIT).await.unwrap();
    assert_eq!(
        event["missed"], 3,
        "only the visible project's event arrives"
    );
}

#[tokio::test]
async fn lagging_subscribers_get_a_resync() {
    let h = harness_with(config(), |b| b.with_events(EventBus::new(2)));
    let token = h.token(user(1), &[TokenScope::Read]);
    let mut reader = open(&h, &token).await;
    assert_eq!(reader.next(WAIT).await.unwrap()["type"], "heartbeat");
    for i in 0..10 {
        h.state
            .events()
            .publish(EventScope::Organization, resync(100 + i));
    }
    let event = reader.next(WAIT).await.unwrap();
    assert_eq!(event["type"], "resync");
    assert_eq!(event["missed"], 8, "{event}");
    assert_eq!(reader.next(WAIT).await.unwrap()["missed"], 108);
    assert_eq!(reader.next(WAIT).await.unwrap()["missed"], 109);
}

#[tokio::test]
async fn stream_needs_a_read_scope() {
    let h = harness();
    let write_only = h.token(user(1), &[TokenScope::Write]);
    let reply = h
        .send(get("/api/v1/events").bearer(&write_only).build())
        .await;
    reply.problem(StatusCode::FORBIDDEN, "forbidden");
}
