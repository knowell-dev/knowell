//! Store-backed endpoints against a real PostgreSQL with pgvector.
//!
//! Needs `KNOWELL_TEST_DATABASE_URL` (an admin connection URL); each test
//! creates and drops its own database. Without the variable every test
//! prints one skip line and passes, like knowell-store's own tests.

use std::str::FromStr;
use std::time::Duration;

use axum::http::StatusCode;
use knowell_auth::{ResourceScope, Role, TokenScope};
use knowell_core::TrackTarget;
use knowell_server::{EventScope, ServerConfig};
use knowell_store::hierarchy::{self, Project};
use knowell_store::jobs::{self, Backoff, NewJob};
use knowell_store::views::{self, View};
use knowell_store::{JobId, PgConnectOptions, SourceKind, Store, StoreOptions};
use serde_json::json;
use sqlx::{ConnectOptions, Connection};

use crate::common::*;
use crate::webhooks::{github, github_push, secrets};

const ENV: &str = "KNOWELL_TEST_DATABASE_URL";

pub(crate) struct TestDb {
    pub(crate) store: Store,
    _guard: DropDatabase,
}

impl TestDb {
    pub(crate) async fn create(test: &str) -> Option<Self> {
        let Ok(url) = std::env::var(ENV) else {
            eprintln!("skipping {test}: {ENV} is not set");
            return None;
        };
        let admin = PgConnectOptions::from_str(&url)
            .unwrap_or_else(|_| panic!("{ENV} is not a valid postgres url"));
        let name = format!("knowell_test_{}", uuid::Uuid::now_v7().simple());
        let mut conn = admin
            .connect()
            .await
            .unwrap_or_else(|e| panic!("cannot reach the test server: {e}"));
        sqlx::query(sqlx::AssertSqlSafe(format!("CREATE DATABASE \"{name}\"")))
            .execute(&mut conn)
            .await
            .unwrap();
        conn.close().await.unwrap();
        let guard = DropDatabase {
            admin: admin.clone(),
            name: name.clone(),
        };
        let options = StoreOptions {
            max_connections: 8,
            acquire_timeout: Duration::from_secs(30),
            application_name: "knowell-server-tests".into(),
            ..StoreOptions::default()
        };
        let store = Store::connect_with(admin.database(&name), &options)
            .await
            .unwrap();
        store.migrate().await.unwrap();
        Some(Self {
            store,
            _guard: guard,
        })
    }
}

struct DropDatabase {
    admin: PgConnectOptions,
    name: String,
}

impl Drop for DropDatabase {
    fn drop(&mut self) {
        let admin = self.admin.clone();
        let name = self.name.clone();
        let handle = std::thread::spawn(move || {
            let Ok(rt) = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            else {
                return;
            };
            rt.block_on(async move {
                if let Ok(mut conn) = admin.connect().await {
                    let _ = sqlx::query(sqlx::AssertSqlSafe(format!(
                        "DROP DATABASE IF EXISTS \"{name}\" WITH (FORCE)"
                    )))
                    .execute(&mut conn)
                    .await;
                    let _ = conn.close().await;
                }
            });
        });
        let _ = handle.join();
    }
}

macro_rules! require_db {
    () => {
        match crate::store::TestDb::create(module_path!()).await {
            Some(db) => db,
            None => return,
        }
    };
}
pub(crate) use require_db;

fn commit(c: char) -> String {
    c.to_string().repeat(40)
}

/// `acme` / `main` with projects `api` (active generation, newer commit
/// seen: stale) and `web` (building), a queued job and a dead job.
struct Seed {
    api: Project,
    api_view: View,
    web_view: View,
    dead_job: JobId,
    queued_job: JobId,
}

async fn seed(store: &Store) -> Seed {
    let mut c = store.acquire().await.unwrap();
    let org = hierarchy::create_organization(&mut c, &n("acme"))
        .await
        .unwrap();
    let ws = hierarchy::create_workspace(&mut c, org.id, &n("main"))
        .await
        .unwrap();
    let api_src = hierarchy::create_source(&mut c, org.id, SourceKind::Git, "/repos/api")
        .await
        .unwrap();
    let web_src = hierarchy::create_source(&mut c, org.id, SourceKind::Directory, "/srv/web")
        .await
        .unwrap();
    let api = hierarchy::create_project(&mut c, ws.id, api_src.id, &n("api"), None)
        .await
        .unwrap();
    let web = hierarchy::create_project(&mut c, ws.id, web_src.id, &n("web"), None)
        .await
        .unwrap();
    let main: TrackTarget = "branch:main".parse().unwrap();
    let api_view = views::create_view(&mut c, api.id, &main).await.unwrap();
    let g = views::begin_generation(&mut c, api_view.id, Some(&commit('a')))
        .await
        .unwrap();
    views::activate_generation(&mut c, api_view.id, g)
        .await
        .unwrap();
    views::record_seen_commit(&mut c, api_view.id, &commit('b'))
        .await
        .unwrap();
    let web_view = views::create_view(&mut c, web.id, &main).await.unwrap();
    views::begin_generation(&mut c, web_view.id, None)
        .await
        .unwrap();

    let queued = jobs::enqueue(
        &mut c,
        &NewJob::new("index_view", json!({"projectName": "api"})),
    )
    .await
    .unwrap();
    let mut doomed = NewJob::new(
        "embed_chunks",
        json!({"projectName": "web", "workspaceName": "main"}),
    );
    doomed.max_attempts = 1;
    let dead = jobs::enqueue(&mut c, &doomed).await.unwrap();
    // Claim only the doomed job's kind, then fail it into the dead-letter queue.
    let claimed = jobs::claim(
        &mut c,
        "test-worker",
        &["embed_chunks"],
        Duration::from_secs(30),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(claimed.id, dead.id);
    jobs::fail(
        &mut c,
        dead.id,
        "test-worker",
        "synthetic failure",
        &Backoff::default(),
    )
    .await
    .unwrap();
    Seed {
        api,
        api_view: views::get_view(&mut c, api_view.id).await.unwrap().unwrap(),
        web_view,
        dead_job: dead.id,
        queued_job: queued.id,
    }
}

fn harness_for(db: &TestDb, cfg: ServerConfig) -> Harness {
    let store = db.store.clone();
    harness_with(cfg, move |b| {
        b.with_store(store).with_webhook_secrets(secrets())
    })
}

#[tokio::test]
async fn health_reports_database_and_queue() {
    let db = require_db!();
    let s = seed(&db.store).await;
    let h = harness_for(&db, config());
    let token = h.token(user(1), &[TokenScope::Read]);
    let body = h
        .send(get("/api/v1/health").bearer(&token).build())
        .await
        .json();
    let database = body["components"]
        .as_array()
        .unwrap()
        .iter()
        .find(|c| c["name"] == "database")
        .unwrap()
        .clone();
    assert_eq!(database["status"], "ok", "{body}");
    assert_eq!(body["status"], "degraded", "no engine is wired");
    assert_eq!(body["queue"]["queued"], 1);
    assert_eq!(body["queue"]["deadLetter"], 1);
    assert_eq!(body["queue"]["running"], 0);
    assert!(body["queue"]["oldestQueuedMs"].as_u64().is_some());
    let _ = s;
}

#[tokio::test]
async fn organization_must_exist() {
    let db = require_db!();
    let h = harness_for(&db, config());
    let token = h.admin_token();
    h.send(get("/api/v1/workspaces").bearer(&token).build())
        .await
        .problem(StatusCode::SERVICE_UNAVAILABLE, "not_initialized");
}

#[tokio::test]
async fn workspaces_and_projects_respect_visibility() {
    let db = require_db!();
    let s = seed(&db.store).await;
    let h = harness_for(&db, config());
    let admin = h.admin_token();
    let list = h
        .send(get("/api/v1/workspaces").bearer(&admin).build())
        .await
        .json();
    assert_eq!(list.as_array().unwrap().len(), 1);
    assert_eq!(list[0]["name"], "main");
    assert_eq!(list[0]["projectCount"], 2);
    assert!(list[0]["memberCount"].is_null());
    assert!(
        list[0]["trackedRef"].is_null(),
        "no knowell.toml configured"
    );
    let ws_id = list[0]["id"].as_str().unwrap().to_owned();

    let detail = h
        .send(
            get(&format!("/api/v1/workspaces/{ws_id}"))
                .bearer(&admin)
                .build(),
        )
        .await
        .json();
    assert_eq!(detail["projectIds"].as_array().unwrap().len(), 2);

    let projects = h
        .send(
            get(&format!("/api/v1/projects?workspace={ws_id}"))
                .bearer(&admin)
                .build(),
        )
        .await
        .json();
    let names: Vec<&str> = projects
        .as_array()
        .unwrap()
        .iter()
        .map(|p| p["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["api", "web"]);
    assert_eq!(projects[0]["indexed"], true);
    assert!(projects[0]["lastIndexedAt"].is_string());
    assert_eq!(projects[1]["indexed"], false);
    assert!(projects[0]["fileCount"].is_null());

    // A viewer of `api` only sees `api`.
    h.grant(
        user(2),
        Role::Viewer,
        ResourceScope::project(n("main"), n("api")),
    );
    let viewer = h.token(user(2), &[TokenScope::Read]);
    let list = h
        .send(get("/api/v1/workspaces").bearer(&viewer).build())
        .await
        .json();
    assert_eq!(list[0]["projectCount"], 1);
    let projects = h
        .send(get("/api/v1/projects").bearer(&viewer).build())
        .await
        .json();
    assert_eq!(projects.as_array().unwrap().len(), 1);
    let web_id = h
        .send(get("/api/v1/projects").bearer(&admin).build())
        .await
        .json()[1]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    h.send(
        get(&format!("/api/v1/projects/{web_id}"))
            .bearer(&viewer)
            .build(),
    )
    .await
    .problem(StatusCode::NOT_FOUND, "not_found");
    // Nobody sees nothing (an empty list, not an error).
    let nobody = h.token(user(3), &[TokenScope::Read]);
    let list = h
        .send(get("/api/v1/workspaces").bearer(&nobody).build())
        .await
        .json();
    assert_eq!(list, json!([]));
    h.send(
        get(&format!("/api/v1/workspaces/{ws_id}"))
            .bearer(&nobody)
            .build(),
    )
    .await
    .problem(StatusCode::NOT_FOUND, "not_found");
    // Malformed and unknown ids.
    for path in [
        "/api/v1/projects/not-a-uuid",
        "/api/v1/projects/0190d1c4-0000-7000-8000-000000000001",
    ] {
        h.send(get(path).bearer(&admin).build())
            .await
            .problem(StatusCode::NOT_FOUND, "not_found");
    }
    let _ = s;
}

#[tokio::test]
async fn project_detail_with_settings_provenance() {
    let db = require_db!();
    let s = seed(&db.store).await;
    let tmp = tempfile::tempdir().unwrap();
    let file = tmp.path().join("knowell.toml");
    std::fs::write(
        &file,
        r#"version = 1
[workspace]
name = "main"
description = "Synthetic test workspace"
track = "branch:main"
exclude = ["**/generated/**"]

[[project]]
name = "api"
path = "api"
data_policy = "cloud"
exclude = ["fixtures/**"]
embedding = { preset = "compact" }

[[project]]
name = "web"
path = "web"
track = "tag:v1.0.0"
"#,
    )
    .unwrap();
    let mut cfg = config();
    cfg.workspace_files.insert(n("main"), file.clone());
    let h = harness_for(&db, cfg);
    let admin = h.admin_token();
    let detail = h
        .send(
            get(&format!("/api/v1/projects/{}", s.api.id))
                .bearer(&admin)
                .build(),
        )
        .await;
    assert_eq!(detail.status, StatusCode::OK, "{}", detail.text());
    let d = detail.json();
    assert_eq!(d["name"], "api");
    assert_eq!(d["source"], json!({"type": "git", "remote": "/repos/api"}));
    assert_eq!(d["root"]["value"], "");
    assert_eq!(d["root"]["origin"], "builtin");
    assert_eq!(d["trackedRef"]["value"]["kind"], "branch");
    assert_eq!(d["trackedRef"]["value"]["name"], "main");
    assert_eq!(d["trackedRef"]["origin"], "workspace");
    assert_eq!(
        d["dataPolicy"],
        json!({"value": "cloud", "origin": "project", "originNote": "project \"api\" in knowell.toml"})
    );
    assert_eq!(d["excludes"][0]["origin"], "workspace");
    assert_eq!(d["excludes"][1]["value"], "fixtures/**");
    assert_eq!(d["excludes"][1]["origin"], "project");
    assert_eq!(d["embedding"]["preset"]["value"], "compact");
    assert_eq!(d["embedding"]["dimensions"]["value"], 768);
    assert!(d["embedding"]["provider"].is_null());
    assert_eq!(d["views"][0]["trackTarget"]["text"], "branch:main");
    assert!(d.get("settingsError").is_none());

    let list = h
        .send(get("/api/v1/workspaces").bearer(&admin).build())
        .await
        .json();
    assert_eq!(list[0]["description"], "Synthetic test workspace");
    assert_eq!(list[0]["trackedRef"]["text"], "branch:main");
    assert_eq!(list[0]["dataPolicy"], "local-only");

    // A broken file is reported without details or values.
    std::fs::write(
        &file,
        "version = 1\n[workspace]\nname = \"KNOWELL_CANARY bad name\"\n",
    )
    .unwrap();
    let reply = h
        .send(
            get(&format!("/api/v1/projects/{}", s.api.id))
                .bearer(&admin)
                .build(),
        )
        .await;
    let d = reply.json();
    assert!(d["settingsError"].as_str().unwrap().contains("know doctor"));
    assert!(d["trackedRef"].is_null());
    assert!(!reply.text().contains("CANARY"));
}

#[tokio::test]
async fn indexes_overview_states_jobs_and_visibility() {
    let db = require_db!();
    let s = seed(&db.store).await;
    let h = harness_for(&db, config());
    let admin = h.admin_token();
    let body = h
        .send(get("/api/v1/indexes").bearer(&admin).build())
        .await
        .json();
    let views = body["views"].as_array().unwrap();
    assert_eq!(views.len(), 2);
    assert_eq!(views[0]["projectName"], "api");
    assert_eq!(views[0]["state"], "stale");
    assert_eq!(views[0]["activeIndexCommit"], commit('a'));
    assert_eq!(views[0]["lastSeenCommit"], commit('b'));
    assert_eq!(views[0]["generations"][0]["state"], "active");
    assert_eq!(views[0]["trackTarget"]["kind"], "branch");
    assert_eq!(views[1]["state"], "building");
    assert!(views[1]["generations"][0]["commit"].is_null());
    let jobs = body["jobs"].as_array().unwrap();
    assert_eq!(jobs.len(), 1);
    assert_eq!(jobs[0]["id"], s.queued_job.to_string());
    assert_eq!(jobs[0]["projectName"], "api");
    let dead = body["deadLetters"].as_array().unwrap();
    assert_eq!(dead[0]["jobId"], s.dead_job.to_string());
    assert_eq!(dead[0]["error"], "synthetic failure");
    assert_eq!(dead[0]["workspaceName"], "main");
    assert!(body["migrations"].is_null());

    // A project viewer sees its view, but no organization-wide job data.
    h.grant(
        user(2),
        Role::Viewer,
        ResourceScope::project(n("main"), n("api")),
    );
    let viewer = h.token(user(2), &[TokenScope::Read]);
    let body = h
        .send(get("/api/v1/indexes").bearer(&viewer).build())
        .await
        .json();
    assert_eq!(body["views"].as_array().unwrap().len(), 1);
    assert!(body["jobs"].is_null() && body["deadLetters"].is_null());

    let dead_list = h
        .send(get("/api/v1/jobs?state=dead").bearer(&admin).build())
        .await
        .json();
    assert_eq!(dead_list.as_array().unwrap().len(), 1);
    assert_eq!(dead_list[0]["state"], "dead");
    let all = h
        .send(get("/api/v1/jobs?limit=1").bearer(&admin).build())
        .await
        .json();
    assert_eq!(all.as_array().unwrap().len(), 1);
    for bad in [
        "/api/v1/jobs?state=zombie",
        "/api/v1/jobs?limit=0",
        "/api/v1/jobs?limit=100000",
    ] {
        h.send(get(bad).bearer(&admin).build())
            .await
            .problem(StatusCode::BAD_REQUEST, "invalid_request");
    }
    let _ = s.web_view;
}

#[tokio::test]
async fn reindex_enqueues_idempotently_with_authorization() {
    let db = require_db!();
    let s = seed(&db.store).await;
    let h = harness_for(&db, config());
    let mut events = h.state.events().subscribe();
    let admin = h.admin_token();
    let body = json!({"viewId": s.api_view.id.to_string(), "scope": "full"});
    let reply = h
        .send(
            post("/api/v1/indexes/reindex")
                .bearer(&admin)
                .header("idempotency-key", "click-1")
                .json(&body)
                .build(),
        )
        .await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text());
    let first = reply.json();
    assert_eq!(first["created"], true);
    let again = h
        .send(
            post("/api/v1/indexes/reindex")
                .bearer(&admin)
                .header("idempotency-key", "click-1")
                .json(&body)
                .build(),
        )
        .await
        .json();
    assert_eq!(again["jobId"], first["jobId"]);
    assert_eq!(again["created"], false);

    let job_id = JobId(first["jobId"].as_str().unwrap().parse().unwrap());
    let mut c = db.store.acquire().await.unwrap();
    let job = jobs::get_job(&mut c, job_id).await.unwrap().unwrap();
    assert_eq!(job.kind, knowell_server::JOB_KIND_VIEW_REINDEX);
    assert_eq!(job.payload["scope"], "full");
    assert_eq!(job.payload["projectName"], "api");
    assert_eq!(job.payload["requestedBy"], format!("user:{}", uid(1)));
    assert_eq!(job.priority, 10);
    let event = events.recv().await.unwrap();
    assert_eq!(
        event.scope,
        EventScope::Project {
            workspace: n("main"),
            project: n("api")
        }
    );
    // The allowed state change was audited.
    assert!(h.audit.events().iter().any(|e| e.decision.allowed));

    // A viewer of the project may not reindex it; a stranger cannot see it.
    h.grant(
        user(2),
        Role::Viewer,
        ResourceScope::project(n("main"), n("api")),
    );
    let viewer = h.token(
        user(2),
        &[TokenScope::Read, TokenScope::Write, TokenScope::Admin],
    );
    h.send(
        post("/api/v1/indexes/reindex")
            .bearer(&viewer)
            .json(&body)
            .build(),
    )
    .await
    .problem(StatusCode::FORBIDDEN, "forbidden");
    let stranger = h.token(
        user(3),
        &[TokenScope::Read, TokenScope::Write, TokenScope::Admin],
    );
    h.send(
        post("/api/v1/indexes/reindex")
            .bearer(&stranger)
            .json(&body)
            .build(),
    )
    .await
    .problem(StatusCode::NOT_FOUND, "not_found");
    h.send(
        post("/api/v1/indexes/reindex")
            .bearer(&admin)
            .header("idempotency-key", "has space")
            .json(&body)
            .build(),
    )
    .await
    .problem(StatusCode::BAD_REQUEST, "invalid_request");
    h.send(
        post("/api/v1/indexes/reindex")
            .bearer(&admin)
            .json(&json!({"viewId": s.api_view.id.to_string(), "scope": "everything"}))
            .build(),
    )
    .await
    .problem(StatusCode::BAD_REQUEST, "invalid_request");
}

#[tokio::test]
async fn dead_letters_can_be_retried_once() {
    let db = require_db!();
    let s = seed(&db.store).await;
    let h = harness_for(&db, config());
    let admin = h.admin_token();
    let path = format!("/api/v1/jobs/{}/retry", s.dead_job);
    let reply = h.send(post(&path).bearer(&admin).build()).await;
    assert_eq!(reply.status, StatusCode::NO_CONTENT, "{}", reply.text());
    h.send(post(&path).bearer(&admin).build())
        .await
        .problem(StatusCode::CONFLICT, "not_dead");
    h.send(
        post(&format!("/api/v1/jobs/{}/retry", s.queued_job))
            .bearer(&admin)
            .build(),
    )
    .await
    .problem(StatusCode::CONFLICT, "not_dead");
    h.send(
        post("/api/v1/jobs/0190d1c4-0000-7000-8000-000000000009/retry")
            .bearer(&admin)
            .build(),
    )
    .await
    .problem(StatusCode::NOT_FOUND, "not_found");
    let mut c = db.store.acquire().await.unwrap();
    let job = jobs::get_job(&mut c, s.dead_job).await.unwrap().unwrap();
    assert_eq!(job.state, knowell_store::JobState::Queued);
}

#[tokio::test]
async fn webhook_push_enqueues_a_refresh_once_per_delivery() {
    let db = require_db!();
    let _s = seed(&db.store).await;
    let h = harness_for(&db, config());
    let body = github_push();
    let reply = h.send(github(&body, "delivery-1", "push").build()).await;
    assert_eq!(reply.status, StatusCode::ACCEPTED, "{}", reply.text());
    let first = reply.json();
    assert_eq!(first["accepted"], true);
    assert_eq!(first["created"], true);
    // A redelivery (or replay) of the same delivery id: same job, nothing new.
    let replay = h
        .send(github(&body, "delivery-1", "push").build())
        .await
        .json();
    assert_eq!(replay["jobId"], first["jobId"]);
    assert_eq!(replay["created"], false);
    let other = h
        .send(github(&body, "delivery-2", "push").build())
        .await
        .json();
    assert_ne!(other["jobId"], first["jobId"]);

    let id = JobId(first["jobId"].as_str().unwrap().parse().unwrap());
    let mut c = db.store.acquire().await.unwrap();
    let job = jobs::get_job(&mut c, id).await.unwrap().unwrap();
    assert_eq!(job.kind, knowell_server::JOB_KIND_SOURCE_REFRESH);
    assert_eq!(
        job.idempotency_key.as_deref(),
        Some("webhook:github:delivery-1")
    );
    assert_eq!(
        job.payload,
        json!({
            "provider": "github",
            "deliveryId": "delivery-1",
            "repository": "octo/widgets",
            "repositoryUrls": ["https://github.example/octo/widgets.git"],
            "ref": "refs/heads/main",
            "before": "0".repeat(40),
            "after": "a".repeat(40),
        })
    );
}
