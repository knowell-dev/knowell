//! Push webhooks from GitHub, GitLab and Gitea.
//!
//! Each provider has one shared secret ([`WebhookSecrets`]); a provider
//! without a secret has no endpoint. A delivery is processed in this order:
//!
//! 1. read the body (bounded by `Limits::webhook_body_bytes`);
//! 2. verify it: GitHub `X-Hub-Signature-256` and Gitea `X-Gitea-Signature`
//!    are HMAC-SHA256 of the raw body (verified in constant time); GitLab's
//!    `X-Gitlab-Token` is compared in constant time (as SHA-256 digests, so
//!    the comparison does not leak the secret's length);
//! 3. require a delivery id (GitHub `X-GitHub-Delivery`, Gitea
//!    `X-Gitea-Delivery`, GitLab `Idempotency-Key` or `X-Gitlab-Event-UUID`);
//! 4. accept only push events (GitHub `ping` is answered and ignored);
//! 5. parse just the repository identity, ref and before/after commits;
//! 6. enqueue a `source.refresh` job with idempotency key
//!    `webhook:<provider>:<delivery id>`, so redeliveries and replays of the
//!    same delivery return the existing job.
//!
//! The payload is trusted only to name the repository to refresh: the
//! indexer maps it to registered sources and re-reads git itself. Signatures
//! cover the body but not the delivery id, so a captured body replayed with a
//! new delivery id can at most cause a redundant refresh.

use axum::Json;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use hmac::{Hmac, KeyInit, Mac};
use knowell_store::hierarchy;
use knowell_store::jobs::{self, JobScope, NewJob};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use sha2::{Digest, Sha256};
use subtle::ConstantTimeEq;

use crate::error::{ApiError, ServerError};
use crate::events::{EventScope, ProgressEvent};
use crate::extract::read_limited;
use crate::state::AppState;
use crate::wire::{JobView, WebhookAck};

/// Job kind enqueued for a verified push. Payload (camelCase JSON):
/// `provider`, `deliveryId`, `repository` (`owner/name`), `repositoryUrls`
/// (clone/web URLs without credentials), `ref`, `before`, `after`.
pub const JOB_KIND_SOURCE_REFRESH: &str = "source.refresh";

const MIN_SECRET_BYTES: usize = 16;
const MAX_DELIVERY_ID: usize = 128;
const MAX_REF: usize = 1024;
const MAX_REPOSITORY: usize = 512;
const MAX_URL: usize = 2048;

/// Shared webhook secrets, one per provider, resolved by the caller from
/// secret references. A provider set to `None` has no webhook endpoint.
#[derive(Clone, Default)]
pub struct WebhookSecrets {
    /// Secret configured on GitHub webhooks.
    pub github: Option<SecretString>,
    /// Secret token configured on GitLab webhooks.
    pub gitlab: Option<SecretString>,
    /// Secret configured on Gitea (and Forgejo) webhooks.
    pub gitea: Option<SecretString>,
}

impl std::fmt::Debug for WebhookSecrets {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WebhookSecrets")
            .field("github", &self.github.is_some())
            .field("gitlab", &self.gitlab.is_some())
            .field("gitea", &self.gitea.is_some())
            .finish()
    }
}

impl WebhookSecrets {
    /// Rejects secrets shorter than 16 bytes (easily guessed).
    pub(crate) fn validate(&self) -> Result<(), ServerError> {
        for (provider, secret) in [
            (Provider::Github, &self.github),
            (Provider::Gitlab, &self.gitlab),
            (Provider::Gitea, &self.gitea),
        ] {
            if secret
                .as_ref()
                .is_some_and(|s| s.expose_secret().len() < MIN_SECRET_BYTES)
            {
                return Err(ServerError::Config(format!(
                    "the {} webhook secret must be at least {MIN_SECRET_BYTES} bytes",
                    provider.as_str()
                )));
            }
        }
        Ok(())
    }

    fn get(&self, provider: Provider) -> Option<&SecretString> {
        match provider {
            Provider::Github => self.github.as_ref(),
            Provider::Gitlab => self.gitlab.as_ref(),
            Provider::Gitea => self.gitea.as_ref(),
        }
    }
}

/// A webhook provider.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Provider {
    Github,
    Gitlab,
    Gitea,
}

impl Provider {
    fn parse(text: &str) -> Option<Self> {
        match text {
            "github" => Some(Self::Github),
            "gitlab" => Some(Self::Gitlab),
            "gitea" => Some(Self::Gitea),
            _ => None,
        }
    }

    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::Github => "github",
            Self::Gitlab => "gitlab",
            Self::Gitea => "gitea",
        }
    }
}

/// `POST /api/v1/webhooks/{provider}`.
pub(crate) async fn receive(
    State(state): State<AppState>,
    Path(provider): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    let not_found = || ApiError::not_found("no webhook endpoint is configured for this provider");
    let provider = Provider::parse(&provider).ok_or_else(not_found)?;
    let secret = state.inner().webhooks.get(provider).ok_or_else(not_found)?;
    let body = read_limited(&headers, body, state.config().limits.webhook_body_bytes).await?;

    if !verify(provider, secret.expose_secret().as_bytes(), &headers, &body) {
        tracing::warn!(provider = provider.as_str(), "webhook signature rejected");
        return Err(ApiError::unauthenticated(
            "invalid_signature",
            "the webhook signature is missing or invalid",
        ));
    }
    let delivery = delivery_id(provider, &headers).ok_or_else(|| {
        ApiError::invalid("the webhook delivery id header is missing or malformed")
    })?;
    match event_kind(provider, &headers) {
        EventKind::Push => {}
        EventKind::Ping => return Ok(ack(StatusCode::OK, None, None, Some("ping"))),
        EventKind::Other => {
            return Ok(ack(StatusCode::OK, None, None, Some("event type ignored")));
        }
    }
    let push = parse_push(provider, &body)
        .ok_or_else(|| ApiError::invalid("the webhook payload is not a well-formed push event"))?;

    let store = state.store()?;
    let mut conn = store.acquire().await?;
    let organization = &state.config().organization;
    let Some(org) = hierarchy::find_organization(&mut conn, organization).await? else {
        return Err(ApiError::unavailable(
            "not_initialized",
            "the organization does not exist yet; run `know init`",
        ));
    };
    let mut job = NewJob::new(
        JOB_KIND_SOURCE_REFRESH,
        serde_json::json!({
            "provider": provider.as_str(),
            "deliveryId": delivery,
            "repository": push.repository,
            "repositoryUrls": push.urls,
            "ref": push.git_ref,
            "before": push.before,
            "after": push.after,
        }),
    );
    job.priority = 5;
    job.idempotency_key = Some(format!("webhook:{}:{delivery}", provider.as_str()));
    let enqueued = jobs::enqueue_scoped(&mut conn, &job, JobScope::Organization(org.id)).await?;
    if enqueued.created
        && let Some(stored) = jobs::get_job(&mut conn, enqueued.id).await?
    {
        state.events().publish(
            EventScope::Organization,
            ProgressEvent::Job {
                job: JobView::from_job(&stored),
            },
        );
    }
    tracing::info!(
        provider = provider.as_str(),
        delivery = %delivery,
        job = %enqueued.id,
        created = enqueued.created,
        "webhook push accepted"
    );
    Ok(ack(
        StatusCode::ACCEPTED,
        Some(enqueued.id.to_string()),
        Some(enqueued.created),
        None,
    ))
}

fn ack(
    status: StatusCode,
    job_id: Option<String>,
    created: Option<bool>,
    reason: Option<&'static str>,
) -> Response {
    let body = WebhookAck {
        accepted: job_id.is_some(),
        job_id,
        created,
        reason,
    };
    (status, Json(body)).into_response()
}

/// The single value of `name`, or `None` when absent, repeated or not text.
fn single_header<'a>(headers: &'a HeaderMap, name: &str) -> Option<&'a str> {
    let mut values = headers.get_all(name).iter();
    let first = values.next()?;
    if values.next().is_some() {
        return None;
    }
    first.to_str().ok()
}

/// Verifies a delivery's authenticity in constant time.
pub(crate) fn verify(provider: Provider, secret: &[u8], headers: &HeaderMap, body: &[u8]) -> bool {
    match provider {
        Provider::Github => single_header(headers, "x-hub-signature-256")
            .and_then(|v| v.strip_prefix("sha256="))
            .is_some_and(|hex| hmac_matches(secret, body, hex)),
        Provider::Gitea => single_header(headers, "x-gitea-signature")
            .map(|v| v.strip_prefix("sha256=").unwrap_or(v))
            .is_some_and(|hex| hmac_matches(secret, body, hex)),
        Provider::Gitlab => single_header(headers, "x-gitlab-token").is_some_and(|presented| {
            let a = Sha256::digest(presented.as_bytes());
            let b = Sha256::digest(secret);
            bool::from(a.as_slice().ct_eq(b.as_slice()))
        }),
    }
}

fn hmac_matches(secret: &[u8], body: &[u8], hex_signature: &str) -> bool {
    let Some(signature) = decode_hex(hex_signature) else {
        return false;
    };
    if signature.len() != 32 {
        return false;
    }
    let Ok(mut mac) = <Hmac<Sha256> as KeyInit>::new_from_slice(secret) else {
        return false;
    };
    mac.update(body);
    mac.verify_slice(&signature).is_ok()
}

fn decode_hex(text: &str) -> Option<Vec<u8>> {
    if !text.len().is_multiple_of(2) {
        return None;
    }
    let digit = |c: u8| match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    };
    text.as_bytes()
        .chunks(2)
        .map(|pair| match pair {
            [hi, lo] => Some((digit(*hi)? << 4) | digit(*lo)?),
            _ => None,
        })
        .collect()
}

/// The delivery id, restricted to `[A-Za-z0-9._:-]{1,128}`.
pub(crate) fn delivery_id(provider: Provider, headers: &HeaderMap) -> Option<String> {
    let raw = match provider {
        Provider::Github => single_header(headers, "x-github-delivery"),
        Provider::Gitea => single_header(headers, "x-gitea-delivery"),
        Provider::Gitlab => single_header(headers, "idempotency-key")
            .or_else(|| single_header(headers, "x-gitlab-event-uuid")),
    }?;
    let ok = !raw.is_empty()
        && raw.len() <= MAX_DELIVERY_ID
        && raw
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b".:_-".contains(&b));
    ok.then(|| raw.to_owned())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EventKind {
    Push,
    Ping,
    Other,
}

pub(crate) fn event_kind(provider: Provider, headers: &HeaderMap) -> EventKind {
    let event = match provider {
        Provider::Github => single_header(headers, "x-github-event"),
        Provider::Gitea => single_header(headers, "x-gitea-event"),
        Provider::Gitlab => single_header(headers, "x-gitlab-event"),
    };
    match (provider, event) {
        (Provider::Github, Some("push")) | (Provider::Gitea, Some("push")) => EventKind::Push,
        (Provider::Gitlab, Some("Push Hook" | "Tag Push Hook")) => EventKind::Push,
        (Provider::Github, Some("ping")) => EventKind::Ping,
        _ => EventKind::Other,
    }
}

/// The parts of a push event the server uses.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PushEvent {
    pub(crate) repository: String,
    pub(crate) urls: Vec<String>,
    pub(crate) git_ref: String,
    pub(crate) before: String,
    pub(crate) after: String,
}

#[derive(Deserialize)]
struct HubPush {
    #[serde(rename = "ref")]
    git_ref: String,
    before: String,
    after: String,
    repository: HubRepository,
}

#[derive(Deserialize)]
struct HubRepository {
    full_name: String,
    #[serde(default)]
    clone_url: Option<String>,
    #[serde(default)]
    ssh_url: Option<String>,
    #[serde(default)]
    html_url: Option<String>,
}

#[derive(Deserialize)]
struct LabPush {
    #[serde(rename = "ref")]
    git_ref: String,
    before: String,
    after: String,
    project: LabProject,
}

#[derive(Deserialize)]
struct LabProject {
    path_with_namespace: String,
    #[serde(default)]
    git_http_url: Option<String>,
    #[serde(default)]
    git_ssh_url: Option<String>,
    #[serde(default)]
    web_url: Option<String>,
}

/// Parses and validates a push payload; `None` for anything malformed.
pub(crate) fn parse_push(provider: Provider, body: &[u8]) -> Option<PushEvent> {
    let (repository, urls, git_ref, before, after) = match provider {
        Provider::Github | Provider::Gitea => {
            let p: HubPush = serde_json::from_slice(body).ok()?;
            let r = p.repository;
            (
                r.full_name,
                [r.clone_url, r.ssh_url, r.html_url],
                p.git_ref,
                p.before,
                p.after,
            )
        }
        Provider::Gitlab => {
            let p: LabPush = serde_json::from_slice(body).ok()?;
            let r = p.project;
            (
                r.path_with_namespace,
                [r.git_http_url, r.git_ssh_url, r.web_url],
                p.git_ref,
                p.before,
                p.after,
            )
        }
    };
    let repository_ok = !repository.is_empty()
        && repository.len() <= MAX_REPOSITORY
        && repository.contains('/')
        && !repository
            .chars()
            .any(|c| c.is_control() || c.is_whitespace());
    let ref_ok = git_ref.starts_with("refs/")
        && git_ref.len() <= MAX_REF
        && !git_ref.chars().any(|c| c.is_control() || c.is_whitespace());
    if !(repository_ok && ref_ok && is_object_id(&before) && is_object_id(&after)) {
        return None;
    }
    let mut kept: Vec<String> = urls.into_iter().flatten().filter(|u| safe_url(u)).collect();
    kept.dedup();
    Some(PushEvent {
        repository,
        urls: kept,
        git_ref,
        before,
        after,
    })
}

/// A full SHA-1 or SHA-256 object id in lowercase hex (all zeros for branch
/// creation or deletion).
fn is_object_id(text: &str) -> bool {
    (text.len() == 40 || text.len() == 64)
        && text
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// Keeps URLs that are plausible repository locations and carry no
/// credentials: `https://`/`http://` without user info, `ssh://`, `git://`,
/// or scp-like `user@host:path`.
fn safe_url(url: &str) -> bool {
    if url.is_empty()
        || url.len() > MAX_URL
        || url.chars().any(|c| c.is_control() || c.is_whitespace())
    {
        return false;
    }
    let lower = url.to_ascii_lowercase();
    if let Some(rest) = lower
        .strip_prefix("https://")
        .or_else(|| lower.strip_prefix("http://"))
    {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        return !authority.is_empty() && !authority.contains('@');
    }
    if let Some(rest) = lower
        .strip_prefix("ssh://")
        .or_else(|| lower.strip_prefix("git://"))
    {
        let authority = rest.split('/').next().unwrap_or_default();
        // A user name is fine; `user:password@` is not.
        return match authority.rsplit_once('@') {
            Some((userinfo, host)) => !userinfo.contains(':') && !host.is_empty(),
            None => !authority.is_empty(),
        };
    }
    // scp-like: user@host:path (no scheme).
    match url.split_once('@') {
        Some((user, rest)) => !user.is_empty() && !user.contains([':', '/']) && rest.contains(':'),
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use axum::http::HeaderValue;

    use super::*;

    const SECRET: &[u8] = b"fake-webhook-secret-0123456789";

    fn sign(body: &[u8]) -> String {
        let mut mac = <Hmac<Sha256> as KeyInit>::new_from_slice(SECRET).unwrap();
        mac.update(body);
        let bytes = mac.finalize().into_bytes();
        bytes.iter().map(|b| format!("{b:02x}")).collect()
    }

    fn headers(pairs: &[(&'static str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.append(*k, HeaderValue::from_str(v).unwrap());
        }
        h
    }

    #[test]
    fn github_signatures() {
        let body = br#"{"zen":"x"}"#;
        let good = format!("sha256={}", sign(body));
        assert!(verify(
            Provider::Github,
            SECRET,
            &headers(&[("x-hub-signature-256", &good)]),
            body
        ));
        let upper = format!("sha256={}", sign(body).to_uppercase());
        assert!(verify(
            Provider::Github,
            SECRET,
            &headers(&[("x-hub-signature-256", &upper)]),
            body
        ));
        // Tampered body, wrong secret, bad formats, duplicates.
        assert!(!verify(
            Provider::Github,
            SECRET,
            &headers(&[("x-hub-signature-256", &good)]),
            b"{}"
        ));
        assert!(!verify(
            Provider::Github,
            b"another-fake-secret-0123456",
            &headers(&[("x-hub-signature-256", &good)]),
            body
        ));
        for bad in [
            "",
            "sha256=",
            "sha1=abcd",
            &sign(body),
            "sha256=zz",
            "sha256=abc",
            &format!("sha256={}00", sign(body)),
        ] {
            assert!(
                !verify(
                    Provider::Github,
                    SECRET,
                    &headers(&[("x-hub-signature-256", bad)]),
                    body
                ),
                "{bad}"
            );
        }
        assert!(!verify(Provider::Github, SECRET, &HeaderMap::new(), body));
        let dup = headers(&[
            ("x-hub-signature-256", &good),
            ("x-hub-signature-256", &good),
        ]);
        assert!(!verify(Provider::Github, SECRET, &dup, body));
    }

    #[test]
    fn gitea_and_gitlab_signatures() {
        let body = b"{}";
        assert!(verify(
            Provider::Gitea,
            SECRET,
            &headers(&[("x-gitea-signature", &sign(body))]),
            body
        ));
        assert!(!verify(
            Provider::Gitea,
            SECRET,
            &headers(&[("x-gitea-signature", &sign(b"x"))]),
            body
        ));
        let token = std::str::from_utf8(SECRET).unwrap();
        assert!(verify(
            Provider::Gitlab,
            SECRET,
            &headers(&[("x-gitlab-token", token)]),
            body
        ));
        assert!(!verify(
            Provider::Gitlab,
            SECRET,
            &headers(&[("x-gitlab-token", "fake-webhook-secret-012345678")]),
            body
        ));
        assert!(!verify(
            Provider::Gitlab,
            SECRET,
            &headers(&[("x-gitlab-token", "")]),
            body
        ));
        assert!(!verify(Provider::Gitlab, SECRET, &HeaderMap::new(), body));
    }

    #[test]
    fn delivery_ids_and_events() {
        let h = headers(&[("x-github-delivery", "72d3162e-cc78-11e3-81ab-4c9367dc0958")]);
        assert!(delivery_id(Provider::Github, &h).is_some());
        for bad in ["", "has space", "a/b", &"x".repeat(129), "kn_\u{e9}"] {
            let h = headers(&[("x-github-delivery", bad)]);
            assert_eq!(delivery_id(Provider::Github, &h), None, "{bad}");
        }
        let lab = headers(&[("x-gitlab-event-uuid", "u-1"), ("idempotency-key", "k-1")]);
        assert_eq!(delivery_id(Provider::Gitlab, &lab).as_deref(), Some("k-1"));
        let lab = headers(&[("x-gitlab-event-uuid", "u-1")]);
        assert_eq!(delivery_id(Provider::Gitlab, &lab).as_deref(), Some("u-1"));

        assert_eq!(
            event_kind(Provider::Github, &headers(&[("x-github-event", "push")])),
            EventKind::Push
        );
        assert_eq!(
            event_kind(Provider::Github, &headers(&[("x-github-event", "ping")])),
            EventKind::Ping
        );
        assert_eq!(
            event_kind(Provider::Github, &headers(&[("x-github-event", "issues")])),
            EventKind::Other
        );
        assert_eq!(
            event_kind(
                Provider::Gitlab,
                &headers(&[("x-gitlab-event", "Tag Push Hook")])
            ),
            EventKind::Push
        );
        assert_eq!(
            event_kind(Provider::Gitea, &headers(&[("x-gitea-event", "push")])),
            EventKind::Push
        );
        assert_eq!(
            event_kind(Provider::Gitea, &HeaderMap::new()),
            EventKind::Other
        );
    }

    fn sha(c: char) -> String {
        c.to_string().repeat(40)
    }

    #[test]
    fn parses_github_push() {
        let body = serde_json::json!({
            "ref": "refs/heads/main",
            "before": sha('0'),
            "after": sha('a'),
            "repository": {
                "full_name": "octo/widgets",
                "clone_url": "https://github.example/octo/widgets.git",
                "ssh_url": "git@github.example:octo/widgets.git",
                "html_url": "https://user:KNOWELL_CANARY_pw@github.example/octo/widgets",
                "extra": {"nested": [1, 2, 3]}
            },
            "commits": [{"id": "x", "message": "ignored"}]
        });
        let push = parse_push(Provider::Github, body.to_string().as_bytes()).unwrap();
        assert_eq!(push.repository, "octo/widgets");
        assert_eq!(push.git_ref, "refs/heads/main");
        assert_eq!(
            push.urls,
            vec![
                "https://github.example/octo/widgets.git".to_owned(),
                "git@github.example:octo/widgets.git".to_owned(),
            ]
        );
        assert!(push.urls.iter().all(|u| !u.contains("CANARY")));
    }

    #[test]
    fn parses_gitlab_push() {
        let body = serde_json::json!({
            "object_kind": "push",
            "ref": "refs/tags/v1.0.0",
            "before": sha('1'),
            "after": "b".repeat(64),
            "project": {
                "path_with_namespace": "group/sub/app",
                "git_http_url": "https://gitlab.example/group/sub/app.git",
                "git_ssh_url": "ssh://git@gitlab.example/group/sub/app.git",
                "web_url": "https://gitlab.example/group/sub/app"
            }
        });
        let push = parse_push(Provider::Gitlab, body.to_string().as_bytes()).unwrap();
        assert_eq!(push.repository, "group/sub/app");
        assert_eq!(push.urls.len(), 3);
    }

    #[test]
    fn rejects_malformed_and_hostile_payloads() {
        let valid = serde_json::json!({
            "ref": "refs/heads/main", "before": sha('0'), "after": sha('a'),
            "repository": {"full_name": "o/r"}
        });
        assert!(parse_push(Provider::Github, valid.to_string().as_bytes()).is_some());
        let text = valid.to_string();
        // Truncated at every length.
        for cut in 0..text.len() {
            assert!(
                parse_push(Provider::Github, &text.as_bytes()[..cut]).is_none(),
                "{cut}"
            );
        }
        let mutate = |key: &str, value: serde_json::Value| {
            let mut v = valid.clone();
            v[key] = value;
            v.to_string()
        };
        for body in [
            mutate("ref", serde_json::json!("main")),
            mutate("ref", serde_json::json!("refs/heads/a b")),
            mutate(
                "ref",
                serde_json::json!(format!("refs/{}", "x".repeat(1100))),
            ),
            mutate("before", serde_json::json!("abc")),
            mutate("after", serde_json::json!(sha('A'))),
            mutate("after", serde_json::json!(sha('g'))),
            mutate("after", serde_json::json!(5)),
            mutate("repository", serde_json::json!({"full_name": "no-slash"})),
            mutate("repository", serde_json::json!({"full_name": "o/r\n"})),
            mutate("repository", serde_json::json!("o/r")),
            "null".to_owned(),
            "[]".to_owned(),
            "{".repeat(10_000),
        ] {
            assert!(
                parse_push(Provider::Github, body.as_bytes()).is_none(),
                "{body:.80}"
            );
        }
        assert!(parse_push(Provider::Gitlab, text.as_bytes()).is_none());
        assert!(parse_push(Provider::Github, &[0xff, 0xfe]).is_none());
    }

    #[test]
    fn url_filter() {
        for ok in [
            "https://h.example/o/r.git",
            "http://h.example/o/r",
            "ssh://git@h.example/o/r.git",
            "git://h.example/o/r.git",
            "git@h.example:o/r.git",
        ] {
            assert!(safe_url(ok), "{ok}");
        }
        for bad in [
            "",
            "https://user:pw@h.example/o/r",
            "https://token@h.example/o/r",
            "ssh://git:pw@h.example/o/r",
            "file:///etc/passwd",
            "javascript:alert(1)",
            "h.example/o/r",
            "https://",
            "https://h.example/a b",
            "user:pw@h.example:o/r",
        ] {
            assert!(!safe_url(bad), "{bad}");
        }
    }

    #[test]
    fn weak_secrets_rejected() {
        let short = WebhookSecrets {
            github: Some(SecretString::from("short")),
            ..WebhookSecrets::default()
        };
        assert!(short.validate().is_err());
        let ok = WebhookSecrets {
            gitlab: Some(SecretString::from("fake-webhook-secret-0123456789")),
            ..WebhookSecrets::default()
        };
        assert!(ok.validate().is_ok());
        assert!(!format!("{ok:?}").contains("fake"));
    }
}
