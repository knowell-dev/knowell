//! Webhook signature verification and request handling (store-backed flows
//! are in `store.rs`).

use axum::http::StatusCode;
use hmac::{Hmac, KeyInit, Mac};
use knowell_server::{SecretString, WebhookSecrets};
use serde_json::json;
use sha2::Sha256;

use crate::common::*;

pub(crate) const SECRET: &str = "fake-webhook-secret-for-tests-01";

pub(crate) fn secrets() -> WebhookSecrets {
    WebhookSecrets {
        github: Some(SecretString::from(SECRET)),
        gitlab: Some(SecretString::from(SECRET)),
        gitea: Some(SecretString::from(SECRET)),
    }
}

pub(crate) fn sign(body: &[u8]) -> String {
    let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(SECRET.as_bytes()).unwrap();
    mac.update(body);
    mac.finalize()
        .into_bytes()
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub(crate) fn github_push() -> String {
    json!({
        "ref": "refs/heads/main",
        "before": "0".repeat(40),
        "after": "a".repeat(40),
        "repository": {
            "full_name": "octo/widgets",
            "clone_url": "https://github.example/octo/widgets.git"
        }
    })
    .to_string()
}

pub(crate) fn github(body: &str, delivery: &str, event: &str) -> Req {
    post("/api/v1/webhooks/github")
        .header("content-type", "application/json")
        .header("x-github-event", event)
        .header("x-github-delivery", delivery)
        .header(
            "x-hub-signature-256",
            &format!("sha256={}", sign(body.as_bytes())),
        )
        .raw(body.to_owned())
}

#[tokio::test]
async fn unconfigured_and_unknown_providers_do_not_exist() {
    let h = harness();
    let body = github_push();
    let reply = h.send(github(&body, "d-1", "push").build()).await;
    reply.problem(StatusCode::NOT_FOUND, "not_found");
    let h = harness_with(config(), |b| b.with_webhook_secrets(secrets()));
    let reply = h
        .send(post("/api/v1/webhooks/bitbucket").raw("{}").build())
        .await;
    reply.problem(StatusCode::NOT_FOUND, "not_found");
}

#[tokio::test]
async fn invalid_signatures_are_rejected() {
    let h = harness_with(config(), |b| b.with_webhook_secrets(secrets()));
    let body = github_push();
    let tampered = body.replace("main", "evil");
    let req = post("/api/v1/webhooks/github")
        .header("x-github-event", "push")
        .header("x-github-delivery", "d-1")
        .header(
            "x-hub-signature-256",
            &format!("sha256={}", sign(body.as_bytes())),
        )
        .raw(tampered)
        .build();
    let reply = h.send(req).await;
    reply.problem(StatusCode::UNAUTHORIZED, "invalid_signature");
    assert!(!reply.text().contains(SECRET));
    let req = post("/api/v1/webhooks/github")
        .header("x-github-event", "push")
        .header("x-github-delivery", "d-1")
        .raw(body.clone())
        .build();
    h.send(req)
        .await
        .problem(StatusCode::UNAUTHORIZED, "invalid_signature");
    let req = post("/api/v1/webhooks/gitlab")
        .header("x-gitlab-event", "Push Hook")
        .header("x-gitlab-token", "wrong-token-of-some-length")
        .header("x-gitlab-event-uuid", "u-1")
        .raw(body.clone())
        .build();
    h.send(req)
        .await
        .problem(StatusCode::UNAUTHORIZED, "invalid_signature");
    let req = post("/api/v1/webhooks/gitea")
        .header("x-gitea-event", "push")
        .header("x-gitea-delivery", "g-1")
        .header("x-gitea-signature", &sign(b"other"))
        .raw(body)
        .build();
    h.send(req)
        .await
        .problem(StatusCode::UNAUTHORIZED, "invalid_signature");
}

#[tokio::test]
async fn verified_deliveries_without_a_store() {
    let h = harness_with(config(), |b| b.with_webhook_secrets(secrets()));
    let body = github_push();
    // Ping is answered and ignored.
    let reply = h.send(github(&body, "d-ping", "ping").build()).await;
    assert_eq!(reply.status, StatusCode::OK);
    assert_eq!(reply.json(), json!({"accepted": false, "reason": "ping"}));
    // Other events are ignored.
    let reply = h.send(github(&body, "d-issue", "issues").build()).await;
    assert_eq!(reply.json()["accepted"], false);
    // A push is verified and parsed, but nothing can be queued without a database.
    let reply = h.send(github(&body, "d-push", "push").build()).await;
    reply.problem(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable");
    // Missing / malformed delivery ids.
    let reply = h.send(github(&body, "bad id", "push").build()).await;
    reply.problem(StatusCode::BAD_REQUEST, "invalid_request");
    // A signed but malformed payload.
    let bad = json!({"ref": "main"}).to_string();
    let reply = h.send(github(&bad, "d-bad", "push").build()).await;
    reply.problem(StatusCode::BAD_REQUEST, "invalid_request");
    // GitLab's token and Gitea's signature are accepted.
    let lab = json!({
        "ref": "refs/heads/main", "before": "0".repeat(40), "after": "b".repeat(40),
        "project": {"path_with_namespace": "group/app", "git_http_url": "https://gitlab.example/group/app.git"}
    })
    .to_string();
    let req = post("/api/v1/webhooks/gitlab")
        .header("x-gitlab-event", "Push Hook")
        .header("x-gitlab-token", SECRET)
        .header("idempotency-key", "k-1")
        .raw(lab)
        .build();
    h.send(req)
        .await
        .problem(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable");
    let req = post("/api/v1/webhooks/gitea")
        .header("x-gitea-event", "push")
        .header("x-gitea-delivery", "g-1")
        .header("x-gitea-signature", &sign(body.as_bytes()))
        .raw(body.clone())
        .build();
    h.send(req)
        .await
        .problem(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable");
}

#[tokio::test]
async fn webhook_bodies_are_limited_and_need_no_session() {
    let mut cfg = config();
    cfg.limits.webhook_body_bytes = 64;
    let h = harness_with(cfg, |b| b.with_webhook_secrets(secrets()));
    let body = github_push();
    assert!(body.len() > 64);
    let reply = h.send(github(&body, "d-big", "push").build()).await;
    reply.problem(StatusCode::PAYLOAD_TOO_LARGE, "payload_too_large");
    // A browser origin is still checked.
    let reply = h
        .send(
            github(&body, "d-o", "push")
                .header("origin", "http://evil.example")
                .build(),
        )
        .await;
    reply.problem(StatusCode::FORBIDDEN, "origin_not_allowed");
}

#[test]
fn weak_secrets_fail_the_build() {
    let weak = WebhookSecrets {
        gitea: Some(SecretString::from("short")),
        ..WebhookSecrets::default()
    };
    let err = knowell_server::AppState::builder(config())
        .with_webhook_secrets(weak)
        .build()
        .unwrap_err();
    assert!(err.to_string().contains("gitea"));
    assert!(!err.to_string().contains("short"));
}
