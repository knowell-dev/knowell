//! Database-backed tokens, grants and audit log (`StoreTokenStore`,
//! `StoreAuditSink`), and tenant-scoped job listings.
//!
//! Store-backed tests need `KNOWELL_TEST_DATABASE_URL`, like `store.rs`.

use std::sync::Arc;
use std::time::Duration;

use axum::http::StatusCode;
use knowell_auth::{Principal, StoredToken, TokenScope, TokenScopes, issue_token};
use knowell_core::TrackTarget;
use knowell_server::{AppState, ServerError, StoreTokenStore};
use knowell_store::audit::{self, AuditFilter};
use knowell_store::hierarchy::{self, Organization, Workspace};
use knowell_store::identity::{self, GrantScope, NewGrant, NewPrincipal};
use knowell_store::jobs::{self, JobFilter, JobScope, JobScopeFilter, NewJob};
use knowell_store::views::{self, View};
use knowell_store::{ApiTokenId, GrantRole, JobId, PrincipalId, PrincipalKind, SourceKind, Store};
use serde_json::json;
use time::OffsetDateTime;

use crate::common::*;
use crate::store::require_db;
use crate::webhooks::{github, github_push, secrets};

/// `acme` / `main` / `api` with a view; principals alice (user 1, org
/// admin) and bob (user 2, viewer of `api`).
struct Seed {
    org: Organization,
    workspace: Workspace,
    view: View,
}

async fn seed(store: &Store) -> Seed {
    let mut c = store.acquire().await.unwrap();
    let org = hierarchy::create_organization(&mut c, &n("acme"))
        .await
        .unwrap();
    let workspace = hierarchy::create_workspace(&mut c, org.id, &n("main"))
        .await
        .unwrap();
    let source = hierarchy::create_source(&mut c, org.id, SourceKind::Git, "/repos/api")
        .await
        .unwrap();
    let api = hierarchy::create_project(&mut c, workspace.id, source.id, &n("api"), None)
        .await
        .unwrap();
    let main: TrackTarget = "branch:main".parse().unwrap();
    let view = views::create_view(&mut c, api.id, &main).await.unwrap();
    for (k, name) in [(1, "alice"), (2, "bob")] {
        identity::create_principal(
            &mut c,
            org.id,
            &NewPrincipal {
                id: Some(PrincipalId(uid(k).as_uuid())),
                kind: PrincipalKind::User,
                name: n(name),
                display_name: None,
            },
        )
        .await
        .unwrap();
    }
    for (k, role, scope) in [
        (1, GrantRole::Admin, GrantScope::Organization),
        (2, GrantRole::Viewer, GrantScope::Project(api.id)),
    ] {
        identity::create_grant(
            &mut c,
            &NewGrant {
                principal: PrincipalId(uid(k).as_uuid()),
                role,
                scope,
                created_by: None,
            },
        )
        .await
        .unwrap();
    }
    Seed {
        org,
        workspace,
        view,
    }
}

/// A harness whose tokens, grants and audit log live in the database. The
/// memory token store and sink of the common harness are replaced.
fn db_harness(store: &Store) -> Harness {
    let store = store.clone();
    harness_with(config(), move |b| {
        b.with_store(store)
            .with_store_tokens()
            .with_store_audit()
            .with_webhook_secrets(secrets())
    })
}

fn all_scopes() -> TokenScopes {
    TokenScopes::new([TokenScope::Read, TokenScope::Write, TokenScope::Admin]).unwrap()
}

/// Issues a token and saves it in the database; returns the plaintext and
/// the record.
async fn saved_token(
    tokens: &StoreTokenStore,
    principal: Principal,
    scopes: TokenScopes,
    now: OffsetDateTime,
    expires: Option<OffsetDateTime>,
) -> (String, StoredToken) {
    let (plain, stored) = issue_token(&principal, scopes, expires, now, &pepper()).unwrap();
    tokens
        .save_token(&stored, Some("test token"), None)
        .await
        .unwrap();
    (plain.expose().to_owned(), stored)
}

#[tokio::test]
async fn database_tokens_authenticate_with_database_grants() {
    let db = require_db!();
    let _seed = seed(&db.store).await;
    let h = db_harness(&db.store);
    let tokens = StoreTokenStore::new(db.store.clone(), n("acme"));
    let now = OffsetDateTime::now_utc();
    let (alice, alice_record) = saved_token(&tokens, user(1), all_scopes(), now, None).await;
    let (bob, _) = saved_token(&tokens, user(2), TokenScopes::read_only(), now, None).await;

    // A second, revoked record with alice's prefix (prefixes are only 40
    // bits): the right candidate still verifies.
    let (_, mut decoy) =
        issue_token(&user(1), TokenScopes::read_only(), None, now, &pepper()).unwrap();
    decoy.prefix = alice_record.prefix.clone();
    decoy.revoked_at = Some(now);
    tokens.save_token(&decoy, None, None).await.unwrap();
    let mut c = db.store.acquire().await.unwrap();
    let org = hierarchy::find_organization(&mut c, &n("acme"))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(
        identity::tokens_with_prefix(&mut c, org.id, &alice_record.prefix)
            .await
            .unwrap()
            .len(),
        2
    );

    let list = h.send(get("/api/v1/projects").bearer(&alice).build()).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text());
    assert_eq!(list.json().as_array().unwrap().len(), 1);
    // Bob's grant comes from the database too.
    let list = h.send(get("/api/v1/projects").bearer(&bob).build()).await;
    assert_eq!(list.status, StatusCode::OK, "{}", list.text());
    assert_eq!(list.json()[0]["name"], "api");

    // Revocation applies to the next request.
    assert!(
        tokens
            .revoke_token(alice_record.id, OffsetDateTime::now_utc())
            .await
            .unwrap()
    );
    h.send(get("/api/v1/projects").bearer(&alice).build())
        .await
        .problem(StatusCode::UNAUTHORIZED, "token_revoked");

    // Expired.
    let past = now - time::Duration::days(2);
    let (expired, _) = saved_token(
        &tokens,
        user(1),
        TokenScopes::read_only(),
        past,
        Some(past + time::Duration::days(1)),
    )
    .await;
    h.send(get("/api/v1/projects").bearer(&expired).build())
        .await
        .problem(StatusCode::UNAUTHORIZED, "token_expired");

    // A disabled principal's tokens read as revoked.
    identity::set_principal_disabled(&mut c, PrincipalId(uid(2).as_uuid()), Some(now))
        .await
        .unwrap();
    h.send(get("/api/v1/projects").bearer(&bob).build())
        .await
        .problem(StatusCode::UNAUTHORIZED, "token_revoked");

    // A well-formed token nobody stored.
    let (unknown, _) =
        issue_token(&user(1), TokenScopes::read_only(), None, now, &pepper()).unwrap();
    h.send(get("/api/v1/projects").bearer(unknown.expose()).build())
        .await
        .problem(StatusCode::UNAUTHORIZED, "invalid_token");

    // Agent tokens act for their user and keep the agent ceiling.
    let (agent, _) = saved_token(
        &tokens,
        agent_of(1),
        TokenScopes::read_only(),
        now,
        Some(now + time::Duration::hours(1)),
    )
    .await;
    let reply = h.send(get("/api/v1/projects").bearer(&agent).build()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
}

async fn last_used(store: &Store, id: ApiTokenId) -> Option<OffsetDateTime> {
    let mut c = store.acquire().await.unwrap();
    identity::get_api_token(&mut c, id)
        .await
        .unwrap()
        .unwrap()
        .last_used_at
}

#[tokio::test]
async fn token_last_use_is_recorded_and_throttled() {
    let db = require_db!();
    seed(&db.store).await;
    let slow = Arc::new(
        StoreTokenStore::new(db.store.clone(), n("acme"))
            .with_touch_interval(Duration::from_secs(3600)),
    );
    let store = db.store.clone();
    let tokens = slow.clone();
    let h = harness_with(config(), move |b| {
        b.with_store(store).with_token_store(tokens)
    });
    let now = OffsetDateTime::now_utc();
    let (plain, record) = saved_token(&slow, user(1), all_scopes(), now, None).await;
    let id = ApiTokenId(record.id.as_uuid());
    assert_eq!(last_used(&db.store, id).await, None);
    let reply = h.send(get("/api/v1/projects").bearer(&plain).build()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    let first = last_used(&db.store, id).await.unwrap();
    assert!(first >= now - time::Duration::seconds(1));
    // Within the interval nothing is written.
    h.send(get("/api/v1/projects").bearer(&plain).build()).await;
    assert_eq!(last_used(&db.store, id).await, Some(first));

    // With a short interval the next use writes again.
    let quick = Arc::new(
        StoreTokenStore::new(db.store.clone(), n("acme"))
            .with_touch_interval(Duration::from_millis(1)),
    );
    let store = db.store.clone();
    let h = harness_with(config(), move |b| {
        b.with_store(store).with_token_store(quick)
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    let reply = h.send(get("/api/v1/projects").bearer(&plain).build()).await;
    assert_eq!(reply.status, StatusCode::OK, "{}", reply.text());
    assert!(last_used(&db.store, id).await.unwrap() > first);
}

#[tokio::test]
async fn audit_rows_are_written_for_denials_and_allowed_changes() {
    let db = require_db!();
    let s = seed(&db.store).await;
    let h = db_harness(&db.store);
    let tokens = StoreTokenStore::new(db.store.clone(), n("acme"));
    let now = OffsetDateTime::now_utc();
    let (alice, _) = saved_token(&tokens, user(1), all_scopes(), now, None).await;
    let (bob, _) = saved_token(&tokens, user(2), all_scopes(), now, None).await;
    let body = json!({"viewId": s.view.id.to_string(), "scope": "changed"});

    let allowed = h
        .send(
            post("/api/v1/indexes/reindex")
                .bearer(&alice)
                .json(&body)
                .build(),
        )
        .await;
    assert_eq!(allowed.status, StatusCode::ACCEPTED, "{}", allowed.text());
    let denied = h
        .send(
            post("/api/v1/indexes/reindex")
                .bearer(&bob)
                .json(&body)
                .build(),
        )
        .await;
    denied.problem(StatusCode::FORBIDDEN, "forbidden");
    let denied_read = h.send(get("/api/v1/jobs").bearer(&bob).build()).await;
    denied_read.problem(StatusCode::FORBIDDEN, "forbidden");
    // Allowed reads are not audited.
    let read = h.send(get("/api/v1/projects").bearer(&alice).build()).await;
    assert_eq!(read.status, StatusCode::OK);

    h.state.flush_audit().await;
    let mut c = db.store.acquire().await.unwrap();
    let entries = audit::list_audit(
        &mut c,
        &AuditFilter {
            organization: Some(s.org.id),
            ..AuditFilter::new(100)
        },
    )
    .await
    .unwrap();
    assert_eq!(entries.len(), 3, "{entries:?}");
    let request_id = |reply: &Reply| reply.headers["x-request-id"].to_str().unwrap().to_owned();
    let find = |id: String| {
        entries
            .iter()
            .find(|e| e.request_id == id)
            .unwrap_or_else(|| panic!("no audit entry for request {id}"))
    };
    let e = find(request_id(&allowed));
    assert!(e.allowed);
    assert_eq!(e.action, "manage_index");
    assert_eq!(e.resource, "project:main/api");
    assert_eq!(e.reason, "granted");
    assert_eq!(e.actor, format!("user:{}", uid(1)));
    assert_eq!(e.principal, Some(PrincipalId(uid(1).as_uuid())));
    let e = find(request_id(&denied));
    assert!(!e.allowed);
    assert_eq!(e.reason, "denied_insufficient_role");
    assert_eq!(e.principal, Some(PrincipalId(uid(2).as_uuid())));
    let e = find(request_id(&denied_read));
    assert!(!e.allowed);
    assert_eq!(
        (e.action.as_str(), e.resource.as_str()),
        ("read_code", "org")
    );
    // No token text anywhere in the log.
    for e in &entries {
        let row = format!("{e:?}");
        for token in [&alice, &bob] {
            assert!(!row.contains(token.as_str()) && !row.contains(&token[3..20]));
        }
    }
}

#[tokio::test]
async fn job_listings_stay_inside_the_organization() {
    let db = require_db!();
    let s = seed(&db.store).await;
    let h = db_harness(&db.store);
    let tokens = StoreTokenStore::new(db.store.clone(), n("acme"));
    let (alice, _) = saved_token(
        &tokens,
        user(1),
        all_scopes(),
        OffsetDateTime::now_utc(),
        None,
    )
    .await;
    let mut c = db.store.acquire().await.unwrap();
    let other = hierarchy::create_organization(&mut c, &n("other"))
        .await
        .unwrap();
    let foreign = jobs::enqueue_scoped(
        &mut c,
        &NewJob::new("source.refresh", json!({})),
        JobScope::Organization(other.id),
    )
    .await
    .unwrap()
    .id;
    let legacy = jobs::enqueue(&mut c, &NewJob::new("index.sync", json!({})))
        .await
        .unwrap()
        .id;

    // A reindex is attributed to the view's workspace, a webhook push to the
    // organization.
    let reply = h
        .send(
            post("/api/v1/indexes/reindex")
                .bearer(&alice)
                .json(&json!({"viewId": s.view.id.to_string(), "scope": "full"}))
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text());
    let reindex = JobId(reply.json()["jobId"].as_str().unwrap().parse().unwrap());
    let reply = h
        .send(github(&github_push(), "delivery-9", "push").build())
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text());
    let refresh = JobId(reply.json()["jobId"].as_str().unwrap().parse().unwrap());
    let in_workspace = jobs::list_jobs(
        &mut c,
        &JobFilter {
            scope: JobScopeFilter {
                workspace: Some(s.workspace.id),
                ..JobScopeFilter::default()
            },
            ..JobFilter::new(10)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        in_workspace.iter().map(|j| j.id).collect::<Vec<_>>(),
        [reindex]
    );

    let listed = h
        .send(get("/api/v1/jobs").bearer(&alice).build())
        .await
        .json();
    let ids: Vec<String> = listed
        .as_array()
        .unwrap()
        .iter()
        .map(|j| j["id"].as_str().unwrap().to_owned())
        .collect();
    assert!(ids.contains(&reindex.to_string()));
    assert!(ids.contains(&refresh.to_string()));
    assert!(
        ids.contains(&legacy.to_string()),
        "unscoped jobs stay visible"
    );
    assert!(!ids.contains(&foreign.to_string()), "{ids:?}");
    let overview = h
        .send(get("/api/v1/indexes").bearer(&alice).build())
        .await
        .json();
    assert!(
        overview["jobs"]
            .as_array()
            .unwrap()
            .iter()
            .all(|j| j["id"] != foreign.to_string())
    );
}

#[tokio::test]
async fn database_access_needs_a_store() {
    for build in [
        AppState::builder(config()).with_store_tokens().build(),
        AppState::builder(config()).with_store_audit().build(),
    ] {
        match build {
            Err(ServerError::Config(message)) => assert!(message.contains("with_store")),
            other => panic!("expected a configuration error, got {other:?}"),
        }
    }
}
