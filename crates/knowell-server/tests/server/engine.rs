//! Routes delegated to the engine.

use axum::http::{Method, StatusCode};
use knowell_auth::{ResourceScope, Role, TokenScope};
use knowell_config::ServerRole;
use knowell_server::{EngineError, EngineRequest, MemoryAction, ServerConfig};
use serde_json::{Value, json};

use crate::common::*;

/// Every delegated route with a valid request.
fn routes() -> Vec<(Method, &'static str, Option<Value>)> {
    vec![
        (
            Method::POST,
            "/api/v1/search",
            Some(
                json!({"query": "where is auth", "expandGraph": false, "rerank": false, "limit": 5}),
            ),
        ),
        (Method::GET, "/api/v1/graph?mode=hierarchy", None),
        (Method::GET, "/api/v1/graph/insights", None),
        (
            Method::POST,
            "/api/v1/graph/trace",
            Some(json!({"from": "svc.pay"})),
        ),
        (
            Method::POST,
            "/api/v1/graph/impact",
            Some(json!({"target": "src/lib.rs"})),
        ),
        (
            Method::POST,
            "/api/v1/context",
            Some(json!({"task": "fix the bug", "tokenBudget": 4000})),
        ),
        (Method::GET, "/api/v1/domains", None),
        (Method::GET, "/api/v1/glossary", None),
        (Method::GET, "/api/v1/memory", None),
        (
            Method::POST,
            "/api/v1/memory/m-1/decision",
            Some(json!({"id": "m-1", "action": "accept"})),
        ),
        (Method::GET, "/api/v1/tasks", None),
        (Method::GET, "/api/v1/rules", None),
        (Method::GET, "/api/v1/profiles", None),
        (
            Method::GET,
            "/api/v1/profiles/balanced/switch-estimate",
            None,
        ),
        (
            Method::POST,
            "/api/v1/profiles/switch",
            Some(json!({"toProfileId": "balanced"})),
        ),
        (Method::GET, "/api/v1/quality/reports", None),
        (Method::GET, "/api/v1/usage?days=30", None),
        (Method::GET, "/api/v1/integrations", None),
    ]
}

fn call(path: &str, method: Method, body: Option<Value>, token: &str) -> Req {
    let req = request(method, path).bearer(token);
    match body {
        Some(b) => req.json(&b),
        None => req,
    }
}

#[tokio::test]
async fn without_an_engine_every_delegated_route_says_why() {
    let h = harness_with(config(), |b| {
        b.engine_unavailable_reason("role `worker` does not answer queries")
    });
    let token = h.admin_token();
    for (method, path, body) in routes() {
        let reply = h.send(call(path, method, body, &token).build()).await;
        let problem = reply.problem(StatusCode::SERVICE_UNAVAILABLE, "engine_unavailable");
        assert_eq!(
            problem["message"], "role `worker` does not answer queries",
            "{path}"
        );
    }
}

#[tokio::test]
async fn answers_pass_through_with_the_callers_context() {
    let engine = FakeEngine::new(|req| {
        Ok(match req {
            EngineRequest::Search(s) => {
                json!({"queryClass": "behavior", "results": [], "echo": s.query})
            }
            EngineRequest::Graph(_)
            | EngineRequest::Trace(_)
            | EngineRequest::Impact(_)
            | EngineRequest::Context(_)
            | EngineRequest::DecideMemory(_)
            | EngineRequest::SwitchEstimate { .. }
            | EngineRequest::StartSwitch(_)
            | EngineRequest::Usage { .. }
            | EngineRequest::Integrations => json!({"ok": true}),
            _ => json!([]),
        })
    });
    let h = harness_with(config(), |b| b.with_engine(engine.clone()));
    let token = h.admin_token();
    for (method, path, body) in routes() {
        let reply = h.send(call(path, method, body, &token).build()).await;
        assert!(reply.status.is_success(), "{path}: {}", reply.text());
    }
    let calls = engine.calls();
    assert_eq!(calls.len(), routes().len());
    assert!(
        calls
            .iter()
            .all(|c| c.principal == user(1) && c.sees_everything)
    );
    match &calls[0].request {
        EngineRequest::Search(s) => {
            assert_eq!((s.query.as_str(), s.limit), ("where is auth", 5));
        }
        other => panic!("{other:?}"),
    }
    match &calls[9].request {
        EngineRequest::DecideMemory(d) => {
            assert_eq!((d.id.as_str(), d.action), ("m-1", MemoryAction::Accept));
        }
        other => panic!("{other:?}"),
    }
    assert!(matches!(
        calls[16].request,
        EngineRequest::Usage { days: 30 }
    ));
}

#[tokio::test]
async fn invalid_requests_never_reach_the_engine() {
    let engine = FakeEngine::new(|_| Ok(json!({})));
    let h = harness_with(config(), |b| b.with_engine(engine.clone()));
    let token = h.admin_token();
    let bad: Vec<(Method, &str, Option<Value>)> = vec![
        (Method::POST, "/api/v1/search", Some(json!({"query": ""}))),
        (
            Method::POST,
            "/api/v1/search",
            Some(json!({"query": "x", "limit": 1000})),
        ),
        (Method::GET, "/api/v1/graph?mode=sideways", None),
        (Method::GET, "/api/v1/graph", None),
        (Method::GET, "/api/v1/graph?mode=hierarchy&extra=1", None),
        (
            Method::POST,
            "/api/v1/graph/trace",
            Some(json!({"from": "x", "depth": 0})),
        ),
        (Method::POST, "/api/v1/graph/impact", Some(json!({}))),
        (
            Method::POST,
            "/api/v1/context",
            Some(json!({"task": "x", "tokenBudget": 1})),
        ),
        (
            Method::POST,
            "/api/v1/memory/m-1/decision",
            Some(json!({"id": "m-2", "action": "accept"})),
        ),
        (
            Method::POST,
            "/api/v1/memory/m-1/decision",
            Some(json!({"action": "maybe"})),
        ),
        (Method::GET, "/api/v1/usage?days=0", None),
        (Method::GET, "/api/v1/usage?days=many", None),
        (
            Method::POST,
            "/api/v1/profiles/switch",
            Some(json!({"toProfileId": ""})),
        ),
    ];
    for (method, path, body) in bad {
        let reply = h.send(call(path, method, body, &token).build()).await;
        reply.problem(StatusCode::BAD_REQUEST, "invalid_request");
    }
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn engine_errors_and_bad_shapes_map_to_problems() {
    let engine = FakeEngine::new(|req| match req {
        EngineRequest::Domains => Ok(json!({"not": "an array"})),
        EngineRequest::Glossary => Err(EngineError::NotFound {
            what: "glossary".into(),
        }),
        EngineRequest::Rules => Err(EngineError::Forbidden {
            message: "not in this workspace".into(),
        }),
        EngineRequest::Tasks => Err(EngineError::Internal {
            message: "KNOWELL_CANARY_internal".into(),
        }),
        EngineRequest::Profiles => Err(EngineError::Unavailable {
            reason: "index not ready".into(),
        }),
        _ => Ok(json!([])),
    });
    let h = harness_with(config(), |b| b.with_engine(engine));
    let token = h.admin_token();
    let send = |path: &'static str| h.send(get(path).bearer(&token).build());
    send("/api/v1/domains")
        .await
        .problem(StatusCode::BAD_GATEWAY, "engine_bad_response");
    send("/api/v1/glossary")
        .await
        .problem(StatusCode::NOT_FOUND, "not_found");
    send("/api/v1/rules")
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    let reply = send("/api/v1/tasks").await;
    reply.problem(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    assert!(!reply.text().contains("CANARY"));
    let reply = send("/api/v1/profiles").await;
    let body = reply.problem(StatusCode::SERVICE_UNAVAILABLE, "engine_unavailable");
    assert_eq!(body["message"], "index not ready");
}

#[tokio::test]
async fn permissions_before_the_engine() {
    let engine = FakeEngine::new(|_| Ok(json!({"ok": true})));
    let h = harness_with(config(), |b| b.with_engine(engine.clone()));
    // A project viewer may search (the engine filters), but not read
    // organization-wide usage or switch profiles.
    h.grant(
        user(2),
        Role::Viewer,
        ResourceScope::project(n("main"), n("api")),
    );
    let viewer = h.token(
        user(2),
        &[TokenScope::Read, TokenScope::Write, TokenScope::Admin],
    );
    let reply = h
        .send(
            post("/api/v1/search")
                .bearer(&viewer)
                .json(&json!({"query": "x"}))
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(!engine.calls()[0].sees_everything);
    h.send(get("/api/v1/usage?days=7").bearer(&viewer).build())
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    h.send(
        post("/api/v1/profiles/switch")
            .bearer(&viewer)
            .json(&json!({"toProfileId": "p"}))
            .build(),
    )
    .await
    .problem(StatusCode::FORBIDDEN, "forbidden");
    // Nobody without grants reaches the engine.
    let nobody = h.token(user(3), &[TokenScope::Read]);
    h.send(get("/api/v1/memory").bearer(&nobody).build())
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    assert_eq!(engine.calls().len(), 1);
}

#[tokio::test]
async fn admin_overview_outside_the_hub() {
    let h = harness();
    let token = h.token(user(1), &[TokenScope::Read]);
    let reply = h.send(get("/api/v1/admin").bearer(&token).build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(
        reply.json(),
        json!({"available": false, "role": "standalone", "users": [], "tokens": [], "audit": []})
    );

    let hub = ServerConfig::new(
        ServerRole::Hub,
        "127.0.0.1:7420".parse().unwrap(),
        n("acme"),
    );
    let h = harness_with(hub, |b| b);
    let read_only = h.token(user(1), &[TokenScope::Read]);
    h.send(get("/api/v1/admin").bearer(&read_only).build())
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    let admin = h.admin_token();
    h.send(get("/api/v1/admin").bearer(&admin).build())
        .await
        .problem(StatusCode::SERVICE_UNAVAILABLE, "engine_unavailable");
}

#[tokio::test]
async fn health_includes_engine_details() {
    let engine = FakeEngine::new(|req| match req {
        EngineRequest::HealthDetail => Ok(json!({
            "freshness": [{"tier": "T0", "coverage": 1.0, "medianLagMs": 12}],
            "resources": {"cpuPercent": 3.5}
        })),
        _ => Ok(json!({})),
    });
    let h = harness_with(config(), |b| b.with_engine(engine));
    let token = h.token(user(1), &[TokenScope::Read]);
    let reply = h.send(get("/api/v1/health").bearer(&token).build()).await;
    let body = reply.json();
    assert_eq!(body["freshness"][0]["tier"], "T0");
    assert_eq!(body["resources"]["cpuPercent"], 3.5);
    assert!(body["recentErrors"].is_null());
    let engine_component = body["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "engine")
        .unwrap()
        .clone();
    assert_eq!(engine_component["status"], "ok");
}
