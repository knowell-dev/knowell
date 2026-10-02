//! Bearer tokens, session cookies with CSRF, authorization and audit.

use axum::http::StatusCode;
use knowell_auth::{
    Action, DecisionReason, Pepper, Resource, ResourceScope, Role, TokenScope, TokenScopes,
    issue_token,
};
use serde_json::json;
use time::OffsetDateTime;

use crate::common::*;

#[tokio::test]
async fn protected_routes_need_credentials() {
    let h = harness();
    let reply = h.send(get("/api/v1/health").build()).await;
    reply.problem(StatusCode::UNAUTHORIZED, "unauthenticated");
    assert_eq!(
        reply.headers["www-authenticate"],
        "Bearer realm=\"knowell\""
    );
    for path in [
        "/api/v1/workspaces",
        "/api/v1/indexes",
        "/api/v1/events",
        "/api/v1/memory",
    ] {
        h.send(get(path).build())
            .await
            .problem(StatusCode::UNAUTHORIZED, "unauthenticated");
    }
}

#[tokio::test]
async fn valid_token_authenticates() {
    let h = harness();
    let token = h.token(user(1), &[TokenScope::Read]);
    let reply = h.send(get("/api/v1/health").bearer(&token).build()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let body = reply.json();
    assert_eq!(body["role"], "standalone");
    // No database and no engine: reported, never faked.
    assert_eq!(body["status"], "down");
    assert!(body["queue"].is_null());
    let components = body["components"].as_array().unwrap();
    assert!(
        components
            .iter()
            .any(|c| c["name"] == "engine" && c["status"] == "down")
    );
    // The scheme is case-insensitive.
    let reply = h
        .send(
            get("/api/v1/health")
                .header("authorization", &format!("bearer {token}"))
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK);
}

#[tokio::test]
async fn expired_revoked_unknown_and_malformed_tokens() {
    let h = harness();
    let now = OffsetDateTime::now_utc();
    let (plain, mut stored) = issue_token(
        &user(1),
        TokenScopes::read_only(),
        Some(now + time::Duration::minutes(5)),
        now,
        &pepper(),
    )
    .unwrap();
    stored.expires_at = Some(now - time::Duration::minutes(1));
    h.tokens.insert_token(stored).unwrap();
    let reply = h
        .send(get("/api/v1/health").bearer(plain.expose()).build())
        .await;
    reply.problem(StatusCode::UNAUTHORIZED, "token_expired");
    assert!(
        reply.headers["www-authenticate"]
            .to_str()
            .unwrap()
            .contains("invalid_token")
    );

    let (plain, stored) =
        issue_token(&user(1), TokenScopes::read_only(), None, now, &pepper()).unwrap();
    let id = stored.id;
    h.tokens.insert_token(stored).unwrap();
    let ok = h
        .send(get("/api/v1/health").bearer(plain.expose()).build())
        .await;
    assert_eq!(ok.status, StatusCode::OK);
    h.tokens.revoke(id, now).unwrap();
    let reply = h
        .send(get("/api/v1/health").bearer(plain.expose()).build())
        .await;
    reply.problem(StatusCode::UNAUTHORIZED, "token_revoked");

    // Well-formed but never stored, or hashed under another pepper.
    let (unknown, _) =
        issue_token(&user(1), TokenScopes::read_only(), None, now, &pepper()).unwrap();
    let reply = h
        .send(get("/api/v1/health").bearer(unknown.expose()).build())
        .await;
    reply.problem(StatusCode::UNAUTHORIZED, "invalid_token");
    let other = Pepper::new(b"another-fake-pepper-0123456789").unwrap();
    let (foreign, stored) =
        issue_token(&user(1), TokenScopes::read_only(), None, now, &other).unwrap();
    h.tokens.insert_token(stored).unwrap();
    let reply = h
        .send(get("/api/v1/health").bearer(foreign.expose()).build())
        .await;
    reply.problem(StatusCode::UNAUTHORIZED, "invalid_token");

    for bad in [
        "kn_short",
        "not-a-token",
        "kn_KNOWELL_CANARY_token_text_that_is_not_valid",
    ] {
        let reply = h.send(get("/api/v1/health").bearer(bad).build()).await;
        reply.problem(StatusCode::UNAUTHORIZED, "invalid_token");
        assert!(!reply.text().contains(bad));
    }
    let reply = h
        .send(
            get("/api/v1/health")
                .header("authorization", "Basic dXNlcjpwYXNz")
                .build(),
        )
        .await;
    reply.problem(StatusCode::UNAUTHORIZED, "invalid_token");
}

#[tokio::test]
async fn tokens_are_rejected_without_a_pepper() {
    let tokens = std::sync::Arc::new(knowell_server::MemoryTokenStore::new());
    let state = knowell_server::AppState::builder(config())
        .with_token_store(tokens.clone())
        .build()
        .unwrap();
    let router = knowell_server::build_router(state);
    let now = OffsetDateTime::now_utc();
    let (plain, stored) =
        issue_token(&user(1), TokenScopes::read_only(), None, now, &pepper()).unwrap();
    tokens.insert_token(stored).unwrap();
    let response =
        tower::ServiceExt::oneshot(router, get("/api/v1/health").bearer(plain.expose()).build())
            .await
            .unwrap();
    assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
}

#[tokio::test]
async fn authorization_denials_are_audited() {
    let h = harness();
    let job = "0190d1c4-0000-7000-8000-000000000001";
    // A read-only token cannot manage indexes even for an admin.
    let read_only = h.token(user(1), &[TokenScope::Read]);
    let reply = h
        .send(
            post(&format!("/api/v1/jobs/{job}/retry"))
                .bearer(&read_only)
                .build(),
        )
        .await;
    let body = reply.problem(StatusCode::FORBIDDEN, "forbidden");
    assert!(body["message"].as_str().unwrap().contains("scope"));

    // A viewer cannot, whatever the token scope.
    h.grant(user(2), Role::Viewer, ResourceScope::Organization);
    let viewer = h.token(
        user(2),
        &[TokenScope::Read, TokenScope::Write, TokenScope::Admin],
    );
    let reply = h
        .send(
            post(&format!("/api/v1/jobs/{job}/retry"))
                .bearer(&viewer)
                .build(),
        )
        .await;
    reply.problem(StatusCode::FORBIDDEN, "forbidden");

    // Somebody without any grant cannot even list jobs.
    let nobody = h.token(user(3), &[TokenScope::Read]);
    let reply = h.send(get("/api/v1/jobs").bearer(&nobody).build()).await;
    reply.problem(StatusCode::FORBIDDEN, "forbidden");

    let events = h.audit.events();
    assert_eq!(events.len(), 3, "{events:?}");
    assert!(events.iter().all(|e| !e.decision.allowed));
    assert_eq!(events[0].action, Action::ManageIndex);
    assert_eq!(events[0].decision.reason, DecisionReason::DeniedTokenScope);
    assert_eq!(
        events[1].decision.reason,
        DecisionReason::DeniedInsufficientRole
    );
    assert_eq!(events[1].actor, user(2));
    assert_eq!(events[2].action, Action::ReadCode);
    assert_eq!(events[2].resource, Resource::Organization);
    assert_eq!(events[2].decision.reason, DecisionReason::DeniedNoGrant);
    // Audit lines hold no token text.
    for e in &events {
        let line = e.to_json_line().unwrap();
        assert!(!line.contains(&read_only) && !line.contains(&viewer));
    }
}

#[tokio::test]
async fn allowed_state_changes_are_audited() {
    let engine = FakeEngine::new(|_| Ok(json!({"started": true})));
    let h = harness_with(config(), |b| b.with_engine(engine));
    let token = h.admin_token();
    let reply = h
        .send(
            post("/api/v1/profiles/switch")
                .bearer(&token)
                .json(&json!({"toProfileId": "balanced-1536"}))
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text());
    let events = h.audit.events();
    assert_eq!(events.len(), 1);
    assert!(events[0].decision.allowed);
    assert_eq!(events[0].action, Action::ManageProviders);
    // Reads are not audited.
    let reply = h.send(get("/api/v1/health").bearer(&token).build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(h.audit.events().len(), 1);
}

#[tokio::test]
async fn agents_stay_below_their_user() {
    let engine = FakeEngine::new(|_| Ok(json!({"id": "m1"})));
    let h = harness_with(config(), |b| b.with_engine(engine.clone()));
    let token = h.token(agent_of(1), &[TokenScope::Read, TokenScope::Write]);
    // Reading works with the user's grants.
    let reply = h
        .send(
            post("/api/v1/search")
                .bearer(&token)
                .json(&json!({"query": "x"}))
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    // Accepting memory is a human review step.
    let reply = h
        .send(
            post("/api/v1/memory/m1/decision")
                .bearer(&token)
                .json(&json!({"action": "accept"}))
                .build(),
        )
        .await;
    let body = reply.problem(StatusCode::FORBIDDEN, "forbidden");
    assert!(body["message"].as_str().unwrap().contains("agents"));
    assert_eq!(engine.calls().len(), 1);
    assert_eq!(engine.calls()[0].principal, agent_of(1));
}

#[tokio::test]
async fn cookie_mutations_need_origin_and_csrf() {
    let h = harness();
    let (cookie, csrf) = h.session().await;
    // Missing CSRF header.
    let reply = h
        .send(
            post("/api/v1/session/logout")
                .cookie(&cookie)
                .origin()
                .build(),
        )
        .await;
    reply.problem(StatusCode::FORBIDDEN, "csrf_missing");
    // Malformed and tampered tokens.
    for bad in ["nonsense", "aaaa.bbbb", &format!("{csrf}x")] {
        let reply = h
            .send(
                post("/api/v1/session/logout")
                    .cookie(&cookie)
                    .origin()
                    .csrf(bad)
                    .build(),
            )
            .await;
        reply.problem(StatusCode::FORBIDDEN, "csrf_invalid");
    }
    // A token of another session.
    let other = harness_session_token(&h).await;
    let reply = h
        .send(
            post("/api/v1/session/logout")
                .cookie(&cookie)
                .origin()
                .csrf(&other)
                .build(),
        )
        .await;
    reply.problem(StatusCode::FORBIDDEN, "csrf_invalid");
    // Missing origin on a cookie mutation.
    let reply = h
        .send(
            post("/api/v1/session/logout")
                .cookie(&cookie)
                .csrf(&csrf)
                .build(),
        )
        .await;
    reply.problem(StatusCode::FORBIDDEN, "origin_required");
    // Reads need neither.
    let reply = h.send(get("/api/v1/health").cookie(&cookie).build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    // Everything right: logged out.
    let reply = h
        .send(
            post("/api/v1/session/logout")
                .cookie(&cookie)
                .origin()
                .csrf(&csrf)
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    let cleared = reply.headers["set-cookie"].to_str().unwrap();
    assert!(cleared.contains("Max-Age=0"));
    let reply = h.send(get("/api/v1/health").cookie(&cookie).build()).await;
    reply.problem(StatusCode::UNAUTHORIZED, "unauthenticated");
}

async fn harness_session_token(h: &Harness) -> String {
    let (_, csrf) = h.session().await;
    csrf
}

#[tokio::test]
async fn bearer_mutations_need_no_origin_or_csrf() {
    let h = harness();
    let token = h.admin_token();
    // Reaches the handler (no engine: 503), so authentication passed.
    let reply = h
        .send(
            post("/api/v1/search")
                .bearer(&token)
                .json(&json!({"query": "x"}))
                .build(),
        )
        .await;
    reply.problem(StatusCode::SERVICE_UNAVAILABLE, "engine_unavailable");
}

#[tokio::test]
async fn bearer_takes_precedence_over_cookie() {
    let h = harness();
    let (cookie, _) = h.session().await;
    let reply = h
        .send(
            get("/api/v1/health")
                .cookie(&cookie)
                .bearer("kn_invalid")
                .build(),
        )
        .await;
    reply.problem(StatusCode::UNAUTHORIZED, "invalid_token");
}
