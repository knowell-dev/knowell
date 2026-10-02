//! Session bootstrap, cookie attributes, token login and expiry.

use std::time::Duration;

use axum::body::Body;
use axum::http::{Request, StatusCode};
use knowell_auth::TokenScope;
use knowell_config::ServerRole;
use knowell_server::ServerConfig;
use serde_json::json;

use crate::common::*;

#[tokio::test]
async fn local_bootstrap_sets_a_strict_cookie() {
    let h = harness();
    let reply = h.send(get("/api/v1/session").build()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let cookie = reply.headers["set-cookie"].to_str().unwrap().to_owned();
    assert!(cookie.starts_with("knowell_session="), "{cookie}");
    for attr in ["; Path=/", "; HttpOnly", "; SameSite=Strict", "; Max-Age="] {
        assert!(cookie.contains(attr), "{cookie}");
    }
    assert!(!cookie.contains("Secure"), "plain http must not set Secure");
    let body = reply.json();
    assert_eq!(body["role"], "standalone");
    assert_eq!(body["user"], format!("user:{}", uid(1)));
    assert!(body["csrfToken"].as_str().unwrap().contains('.'));
    assert!(body["expiresAt"].is_string());

    // With the cookie: same session, fresh CSRF token, no new cookie.
    let pair = cookie.split(';').next().unwrap();
    let again = h.send(get("/api/v1/session").cookie(pair).build()).await;
    assert_eq!(again.status, StatusCode::OK);
    assert!(!again.headers.contains_key("set-cookie"));
    assert_ne!(again.json()["csrfToken"], body["csrfToken"]);
    // Both CSRF tokens are valid for the session.
    let reply = h
        .send(
            post("/api/v1/session/logout")
                .cookie(pair)
                .origin()
                .csrf(body["csrfToken"].as_str().unwrap())
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT);
}

#[tokio::test]
async fn https_uses_secure_host_prefixed_cookie() {
    let mut cfg = config();
    cfg.public_base_url = Some("https://127.0.0.1:7420".into());
    let h = harness_with(cfg, |b| b);
    let reply = h.send(get("/api/v1/session").build()).await;
    let cookie = reply.headers["set-cookie"].to_str().unwrap();
    assert!(cookie.starts_with("__Host-knowell_session="), "{cookie}");
    assert!(cookie.ends_with("; Secure"));
    let pair = cookie.split(';').next().unwrap();
    let reply = h.send(get("/api/v1/health").cookie(pair).build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    // The plain-http cookie name is not accepted over https.
    let plain = pair.replacen("__Host-", "", 1);
    let reply = h.send(get("/api/v1/health").cookie(&plain).build()).await;
    assert_eq!(reply.status, StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn no_local_session_without_a_local_user_or_on_a_hub() {
    let mut cfg = config();
    cfg.local_user = None;
    let h = harness_with(cfg, |b| b);
    h.send(get("/api/v1/session").build())
        .await
        .problem(StatusCode::UNAUTHORIZED, "unauthenticated");

    let mut hub = ServerConfig::new(
        ServerRole::Hub,
        "127.0.0.1:7420".parse().unwrap(),
        n("acme"),
    );
    hub.local_user = Some(uid(1));
    let h = harness_with(hub, |b| b);
    let reply = h.send(get("/api/v1/session").build()).await;
    let body = reply.problem(StatusCode::UNAUTHORIZED, "unauthenticated");
    assert!(body["message"].as_str().unwrap().contains("login"));
}

fn hub() -> Harness {
    let hub = ServerConfig::new(
        ServerRole::Hub,
        "127.0.0.1:7420".parse().unwrap(),
        n("acme"),
    );
    harness_with(hub, |b| b)
}

#[tokio::test]
async fn token_login_opens_a_scoped_session() {
    let h = hub();
    let token = h.token(user(1), &[TokenScope::Read]);
    // Login needs an allowed origin.
    let reply = h
        .send(
            post("/api/v1/session/login")
                .json(&json!({"token": token}))
                .build(),
        )
        .await;
    reply.problem(StatusCode::FORBIDDEN, "origin_required");
    let reply = h
        .send(
            post("/api/v1/session/login")
                .origin()
                .json(&json!({"token": token}))
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.json()["role"], "hub");
    let cookie = reply.headers["set-cookie"].to_str().unwrap();
    let pair = cookie.split(';').next().unwrap().to_owned();
    let csrf = reply.json()["csrfToken"].as_str().unwrap().to_owned();

    let reply = h.send(get("/api/v1/session").cookie(&pair).build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    // The session cannot exceed the read-only token.
    let reply = h
        .send(
            post("/api/v1/indexes/reindex")
                .cookie(&pair)
                .origin()
                .csrf(&csrf)
                .json(&json!({"viewId": "0190d1c4-0000-7000-8000-000000000001", "scope": "full"}))
                .build(),
        )
        .await;
    reply.problem(StatusCode::FORBIDDEN, "forbidden");
}

#[tokio::test]
async fn login_rejects_bad_and_agent_tokens() {
    let h = hub();
    let reply = h
        .send(
            post("/api/v1/session/login")
                .origin()
                .json(&json!({"token": "kn_KNOWELL_CANARY"}))
                .build(),
        )
        .await;
    reply.problem(StatusCode::UNAUTHORIZED, "invalid_token");
    assert!(!reply.text().contains("CANARY"));
    let agent = h.token(agent_of(1), &[TokenScope::Read]);
    let reply = h
        .send(
            post("/api/v1/session/login")
                .origin()
                .json(&json!({"token": agent}))
                .build(),
        )
        .await;
    reply.problem(StatusCode::FORBIDDEN, "forbidden");
    let reply = h
        .send(
            post("/api/v1/session/login")
                .origin()
                .json(&json!({"token": "x", "extra": 1}))
                .build(),
        )
        .await;
    reply.problem(StatusCode::BAD_REQUEST, "invalid_request");
}

#[tokio::test]
async fn idle_sessions_expire() {
    let mut cfg = config();
    cfg.sessions.idle_timeout = Duration::from_millis(50);
    let h = harness_with(cfg, |b| b);
    let (cookie, _) = h.session().await;
    let reply = h.send(get("/api/v1/health").cookie(&cookie).build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    tokio::time::sleep(Duration::from_millis(120)).await;
    let reply = h.send(get("/api/v1/health").cookie(&cookie).build()).await;
    reply.problem(StatusCode::UNAUTHORIZED, "unauthenticated");
    // Bootstrapping again starts a new session.
    let reply = h.send(get("/api/v1/session").cookie(&cookie).build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.headers.contains_key("set-cookie"));
}

#[tokio::test]
async fn sessions_of_users_without_grants_see_nothing() {
    let mut cfg = config();
    cfg.local_user = Some(uid(9));
    let h = harness_with(cfg, |b| b);
    let (cookie, _) = h.session().await;
    // Authenticated, but no grant: delegated reads are refused.
    let reply = h
        .send(get("/api/v1/graph/insights").cookie(&cookie).build())
        .await;
    reply.problem(StatusCode::FORBIDDEN, "forbidden");
    let req = Request::builder()
        .uri("/api/v1/session")
        .header("host", "localhost:7420")
        .header("cookie", &cookie)
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.send(req).await.status, StatusCode::OK);
}
