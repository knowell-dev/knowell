//! Host / Origin checks, problem+json shape, security headers, body limits,
//! timeouts and the panic catcher.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Method, Request, StatusCode};
use knowell_auth::TokenScope;
use knowell_config::ServerRole;
use knowell_server::{EngineRequest, ServerConfig};
use serde_json::json;

use crate::common::*;

#[tokio::test]
async fn rebinding_hosts_are_rejected() {
    let h = harness();
    for host in [
        "evil.example:7420",
        "evil.example",
        "127.0.0.1.evil.example:7420",
        "localhost:7421",
        "0.0.0.0:7420",
        "127.0.0.2:7420",
    ] {
        let req = Request::builder()
            .uri("/api/v1/health/live")
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        let reply = h.send(req).await;
        reply.problem(StatusCode::FORBIDDEN, "host_not_allowed");
        // The rejection itself carries the security headers.
        assert_eq!(reply.headers["x-content-type-options"], "nosniff");
    }
    // Rebinding also fails for the panel and for a valid-looking origin.
    let req = Request::builder()
        .uri("/")
        .header("host", "evil.example:7420")
        .header("origin", ORIGIN)
        .body(Body::empty())
        .unwrap();
    h.send(req)
        .await
        .problem(StatusCode::FORBIDDEN, "host_not_allowed");
}

#[tokio::test]
async fn loopback_hosts_are_accepted() {
    let h = harness();
    for host in [
        "127.0.0.1:7420",
        "localhost:7420",
        "LOCALHOST:7420",
        "[::1]:7420",
    ] {
        let req = Request::builder()
            .uri("/api/v1/health/live")
            .header("host", host)
            .body(Body::empty())
            .unwrap();
        let reply = h.send(req).await;
        assert_eq!(reply.status, StatusCode::OK, "{host}");
        assert_eq!(reply.json(), json!({"status": "ok"}));
    }
}

#[tokio::test]
async fn missing_or_repeated_host_is_rejected() {
    let h = harness();
    let req = Request::builder()
        .uri("/api/v1/health/live")
        .body(Body::empty())
        .unwrap();
    h.send(req)
        .await
        .problem(StatusCode::BAD_REQUEST, "host_missing");
    let req = Request::builder()
        .uri("/api/v1/health/live")
        .header("host", HOST)
        .header("host", HOST)
        .body(Body::empty())
        .unwrap();
    h.send(req)
        .await
        .problem(StatusCode::BAD_REQUEST, "host_invalid");
}

#[tokio::test]
async fn foreign_or_null_origin_is_rejected_on_every_method() {
    let h = harness();
    for origin in [
        "http://evil.example",
        "null",
        "http://127.0.0.1:7421",
        "https://127.0.0.1:7420",
        "http://127.0.0.1:7420/path",
    ] {
        let reply = h
            .send(get("/api/v1/health/live").header("origin", origin).build())
            .await;
        reply.problem(StatusCode::FORBIDDEN, "origin_not_allowed");
        let token = h.admin_token();
        let reply = h
            .send(
                post("/api/v1/search")
                    .bearer(&token)
                    .header("origin", origin)
                    .json(&json!({"query": "x"}))
                    .build(),
            )
            .await;
        reply.problem(StatusCode::FORBIDDEN, "origin_not_allowed");
    }
    let req = get("/api/v1/health/live")
        .origin()
        .header("origin", "http://evil.example")
        .build();
    h.send(req)
        .await
        .problem(StatusCode::FORBIDDEN, "origin_not_allowed");
    // An allowed origin passes.
    let reply = h.send(get("/api/v1/health/live").origin().build()).await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn public_url_hosts_with_default_ports() {
    let mut cfg = ServerConfig::new(ServerRole::Hub, "0.0.0.0:7420".parse().unwrap(), n("acme"));
    cfg.public_base_url = Some("https://knowell.example".into());
    let h = harness_with(cfg, |b| b);
    for (host, origin) in [
        ("knowell.example", "https://knowell.example"),
        ("knowell.example:443", "https://knowell.example:443"),
    ] {
        let req = Request::builder()
            .uri("/api/v1/health/live")
            .header("host", host)
            .header("origin", origin)
            .body(Body::empty())
            .unwrap();
        let reply = h.send(req).await;
        assert_eq!(reply.status, StatusCode::OK, "{host} {}", reply.text());
        assert_eq!(
            reply.headers["strict-transport-security"],
            "max-age=31536000"
        );
    }
    let req = Request::builder()
        .uri("/api/v1/health/live")
        .header("host", "knowell.example")
        .header("origin", "http://knowell.example")
        .body(Body::empty())
        .unwrap();
    h.send(req)
        .await
        .problem(StatusCode::FORBIDDEN, "origin_not_allowed");
}

#[tokio::test]
async fn problem_json_shape_and_request_ids() {
    let h = harness();
    let a = h.send(get("/api/v1/nope").build()).await;
    let body = a.problem(StatusCode::NOT_FOUND, "not_found");
    assert_eq!(body["title"], "Not Found");
    let b = h.send(get("/api/v1/nope").build()).await;
    assert_ne!(a.headers["x-request-id"], b.headers["x-request-id"]);
    // Router-level 405 becomes a problem too.
    let reply = h.send(get("/api/v1/session/login").build()).await;
    reply.problem(StatusCode::METHOD_NOT_ALLOWED, "method_not_allowed");
    // Success responses carry the id header as well.
    let ok = h.send(get("/api/v1/health/live").build()).await;
    assert!(ok.headers.contains_key("x-request-id"));
    // Unknown top-level API and MCP paths are API 404s, not panel pages.
    for path in ["/api", "/api/v2/health", "/mcp", "/mcp/x"] {
        h.send(get(path).build())
            .await
            .problem(StatusCode::NOT_FOUND, "not_found");
    }
}

#[tokio::test]
async fn security_headers_on_api_responses() {
    let h = harness();
    for req in [
        get("/api/v1/health/live").build(),
        get("/api/v1/health").build(),
        get("/api/v1/missing").build(),
    ] {
        let reply = h.send(req).await;
        let hd = &reply.headers;
        assert_eq!(hd["x-content-type-options"], "nosniff");
        assert_eq!(hd["referrer-policy"], "no-referrer");
        assert_eq!(hd["x-frame-options"], "DENY");
        assert_eq!(hd["cross-origin-opener-policy"], "same-origin");
        assert_eq!(hd["cross-origin-resource-policy"], "same-origin");
        assert_eq!(hd["cache-control"], "no-store");
        let csp = hd["content-security-policy"].to_str().unwrap();
        assert!(csp.contains("default-src 'none'") && csp.contains("frame-ancestors 'none'"));
        // Plain http: no HSTS.
        assert!(!hd.contains_key("strict-transport-security"));
    }
}

#[tokio::test]
async fn body_limits_are_enforced() {
    let mut cfg = config();
    cfg.limits.api_body_bytes = 256;
    let engine = FakeEngine::new(|_| Ok(json!({"results": []})));
    let h = harness_with(cfg, |b| b.with_engine(engine.clone()));
    let token = h.admin_token();
    let big = json!({"query": "x".repeat(400)});
    // Announced by Content-Length.
    let reply = h
        .send(post("/api/v1/search").bearer(&token).json(&big).build())
        .await;
    reply.problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    // Streamed without a length.
    let chunks: Vec<Result<String, std::io::Error>> = vec![
        Ok("{\"query\":\"".into()),
        Ok("y".repeat(400)),
        Ok("\"}".into()),
    ];
    let req = post("/api/v1/search")
        .bearer(&token)
        .header("content-type", "application/json")
        .raw(Body::from_stream(futures::stream::iter(chunks)))
        .build();
    h.send(req)
        .await
        .problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    assert!(engine.calls().is_empty());
    // Within the limit the request reaches the engine.
    let reply = h
        .send(
            post("/api/v1/search")
                .bearer(&token)
                .json(&json!({"query": "x"}))
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

#[tokio::test]
async fn bodies_must_be_json_and_errors_do_not_echo_input() {
    let engine = FakeEngine::new(|_| Ok(json!({})));
    let h = harness_with(config(), |b| b.with_engine(engine.clone()));
    let token = h.admin_token();
    let reply = h
        .send(
            post("/api/v1/search")
                .bearer(&token)
                .header("content-type", "text/plain")
                .raw("{\"query\":\"x\"}")
                .build(),
        )
        .await;
    reply.problem(StatusCode::UNSUPPORTED_MEDIA_TYPE, "unsupported_media_type");
    let canary = "KNOWELL_CANARY_body_value";
    for body in [
        json!({"query": 5, "note": canary}),
        json!({"query": canary, "limit": canary}),
    ] {
        let reply = h
            .send(post("/api/v1/search").bearer(&token).json(&body).build())
            .await;
        reply.problem(StatusCode::BAD_REQUEST, "invalid_request");
        assert!(!reply.text().contains(canary), "{}", reply.text());
    }
    let reply = h
        .send(
            post("/api/v1/search")
                .bearer(&token)
                .header("content-type", "application/json")
                .raw(format!("{{\"query\": \"{canary}"))
                .build(),
        )
        .await;
    reply.problem(StatusCode::BAD_REQUEST, "invalid_request");
    assert!(!reply.text().contains(canary));
    assert!(engine.calls().is_empty());
}

#[tokio::test]
async fn slow_requests_time_out() {
    let mut cfg = config();
    cfg.limits.request_timeout = Duration::from_millis(50);
    let engine = FakeEngine::slow(Duration::from_secs(5));
    let h = harness_with(cfg, |b| b.with_engine(engine));
    let token = h.admin_token();
    let reply = h
        .send(
            post("/api/v1/search")
                .bearer(&token)
                .json(&json!({"query": "x"}))
                .build(),
        )
        .await;
    let body = reply.problem(StatusCode::SERVICE_UNAVAILABLE, "timeout");
    assert!(body["message"].as_str().unwrap().contains("50 ms"));
    assert_eq!(reply.headers["retry-after"], "5");
}

#[tokio::test]
async fn handler_panics_become_500_problems() {
    let engine = FakeEngine::new(|req| match req {
        EngineRequest::Domains => panic!("synthetic engine panic KNOWELL_CANARY_panic"),
        _ => Ok(json!([])),
    });
    let h = harness_with(config(), |b| b.with_engine(engine));
    let token = h.token(user(1), &[TokenScope::Read]);
    let reply = h.send(get("/api/v1/domains").bearer(&token).build()).await;
    reply.problem(StatusCode::INTERNAL_SERVER_ERROR, "internal_error");
    assert!(!reply.text().contains("CANARY"));
    assert_eq!(reply.headers["x-frame-options"], "DENY");
    // The server keeps answering.
    let reply = h
        .send(request(Method::GET, "/api/v1/health/live").build())
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}
