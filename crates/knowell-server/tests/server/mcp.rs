//! The `/mcp` mount behind authentication.

use axum::Router;
use axum::extract::Request;
use axum::http::StatusCode;
use axum::routing::any;
use knowell_auth::TokenScope;
use knowell_server::Authenticated;

use crate::common::*;

/// A stand-in for the MCP Streamable HTTP router: answers on `/mcp` with the
/// authenticated principal.
fn fake_mcp() -> Router {
    async fn echo(req: Request) -> String {
        let who = req
            .extensions()
            .get::<Authenticated>()
            .map(|a| a.principal.to_string())
            .unwrap_or_else(|| "nobody".to_owned());
        format!("{} {} {who}", req.method(), req.uri().path())
    }
    Router::new()
        .route("/mcp", any(echo))
        .route("/elsewhere", any(echo))
}

fn mcp_harness() -> Harness {
    harness_with(config(), |b| b.with_mcp(fake_mcp()))
}

#[tokio::test]
async fn mcp_requires_authentication() {
    let h = mcp_harness();
    h.send(post("/mcp").raw("{}").build())
        .await
        .problem(StatusCode::UNAUTHORIZED, "unauthenticated");
    let token = h.token(user(1), &[TokenScope::Read]);
    let reply = h.send(post("/mcp").bearer(&token).raw("{}").build()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert_eq!(reply.text(), format!("POST /mcp user:{}", uid(1)));
    // Routes of the MCP router outside /mcp are not reachable.
    h.send(get("/elsewhere").bearer(&token).build())
        .await
        .problem(StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn agents_use_mcp_with_their_users_grants() {
    let h = mcp_harness();
    let token = h.token(agent_of(1), &[TokenScope::Read]);
    let reply = h.send(get("/mcp").bearer(&token).build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert!(reply.text().starts_with("GET /mcp agent:"));
    // A principal without grants is refused.
    let nobody = h.token(user(4), &[TokenScope::Read]);
    h.send(get("/mcp").bearer(&nobody).build())
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    // A token without the read scope is refused.
    let write_only = h.token(user(1), &[TokenScope::Write]);
    h.send(get("/mcp").bearer(&write_only).build())
        .await
        .problem(StatusCode::FORBIDDEN, "forbidden");
    let denials = h.audit.events();
    assert_eq!(denials.len(), 2);
    assert!(denials.iter().all(|e| !e.decision.allowed));
}

#[tokio::test]
async fn mcp_cookie_mutations_need_csrf_and_host_checks_apply() {
    let h = mcp_harness();
    let (cookie, csrf) = h.session().await;
    h.send(post("/mcp").cookie(&cookie).origin().raw("{}").build())
        .await
        .problem(StatusCode::FORBIDDEN, "csrf_missing");
    let reply = h
        .send(
            post("/mcp")
                .cookie(&cookie)
                .origin()
                .csrf(&csrf)
                .raw("{}")
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
    let req = axum::http::Request::builder()
        .method("POST")
        .uri("/mcp")
        .header("host", "evil.example:7420")
        .body(axum::body::Body::from("{}"))
        .unwrap();
    h.send(req)
        .await
        .problem(StatusCode::FORBIDDEN, "host_not_allowed");
}

#[tokio::test]
async fn mcp_announced_body_limit() {
    let mut cfg = config();
    cfg.limits.mcp_body_bytes = 16;
    let h = harness_with(cfg, |b| b.with_mcp(fake_mcp()));
    let token = h.token(user(1), &[TokenScope::Read]);
    let reply = h
        .send(post("/mcp").bearer(&token).raw("x".repeat(64)).build())
        .await;
    reply.problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
}
