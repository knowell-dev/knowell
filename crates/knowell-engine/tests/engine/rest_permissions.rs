//! Native REST requests enforce organization authorization before parsing or I/O.

use std::sync::Arc;

use knowell_auth::{
    AgentSessionId, Grant, GrantSet, Principal, RequestId, ResourceScope, Role, TokenScope,
    TokenScopes, visible_projects,
};
use knowell_config::ServerRole;
use knowell_engine::{Engine, EngineSettings};
use knowell_server::{EngineContext, EngineError, EngineRequest, MemoryAuditSink, SwitchRequest};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{TestDb, alice, indexer_config, name, require_db};

const REPORT: &str = include_str!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../../eval/baselines/synthetic-small.json"
));
const PROFILE: Uuid = Uuid::from_u128(0xAE57_1001);

struct Fixture {
    engine: Engine,
    reports: tempfile::TempDir,
    _data: tempfile::TempDir,
}

async fn fixture(db: &TestDb, role: ServerRole) -> Fixture {
    let data = tempfile::tempdir().unwrap();
    let reports = tempfile::tempdir().unwrap();
    std::fs::write(reports.path().join("synthetic-permissions.json"), REPORT).unwrap();
    std::fs::write(
        reports.path().join("synthetic-ignored.txt"),
        "synthetic report directory marker",
    )
    .unwrap();
    let engine = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .settings(EngineSettings {
            role,
            eval_reports_dir: Some(reports.path().to_owned()),
            ..EngineSettings::default()
        })
        .build()
        .await
        .unwrap();
    let mut conn = db.store.acquire().await.unwrap();
    sqlx::query(
        "INSERT INTO embedding_profile (id, organization_id, name, provider, model, dimensions, input_format_version)
         SELECT $1, id, 'synthetic-rest-profile', 'synthetic-unavailable-provider',
                'synthetic-model', 64, 'synthetic-v1' FROM organization WHERE name = 'acme'
         ON CONFLICT (id) DO NOTHING",
    ).bind(PROFILE).execute(&mut *conn).await.unwrap();
    Fixture {
        engine,
        reports,
        _data: data,
    }
}

fn context(
    scope: Option<ResourceScope>,
    role: Role,
    scopes: Option<TokenScopes>,
    agent: bool,
) -> EngineContext {
    let principal = if agent {
        Principal::Agent {
            on_behalf_of: alice(),
            client: name("synthetic-rest-agent"),
            session: AgentSessionId::new(Uuid::from_u128(0xAE57_A6E17)),
        }
    } else {
        Principal::User(alice())
    };
    let mut grants = GrantSet::new();
    if let Some(scope) = scope {
        grants.add(Grant::new(Principal::User(alice()), role, scope).unwrap());
    }
    let visible = visible_projects(&principal, &grants);
    EngineContext {
        principal,
        scopes,
        grants: Arc::new(grants),
        visible,
        request_id: RequestId::new("synthetic-rest-permissions").unwrap(),
        audit: Arc::new(MemoryAuditSink::new()),
    }
}

fn org(role: Role, scopes: Option<TokenScopes>, agent: bool) -> EngineContext {
    context(Some(ResourceScope::Organization), role, scopes, agent)
}

fn read_denied() -> Vec<EngineContext> {
    vec![
        context(
            Some(ResourceScope::project(
                name("synthetic-workspace"),
                name("synthetic-project"),
            )),
            Role::Admin,
            None,
            false,
        ),
        context(
            Some(ResourceScope::workspace(name("synthetic-workspace"))),
            Role::Admin,
            None,
            false,
        ),
        context(None, Role::Admin, None, false),
        org(
            Role::Admin,
            Some(TokenScopes::new([TokenScope::Write]).unwrap()),
            false,
        ),
        org(
            Role::Admin,
            Some(TokenScopes::new([TokenScope::Admin]).unwrap()),
            false,
        ),
    ]
}

fn management_denied() -> Vec<EngineContext> {
    let mut denied = read_denied().into_iter().take(3).collect::<Vec<_>>();
    denied.extend([
        org(Role::Viewer, None, false),
        org(Role::Maintainer, None, false),
        org(Role::Admin, Some(TokenScopes::read_only()), false),
        org(
            Role::Admin,
            Some(TokenScopes::new([TokenScope::Write]).unwrap()),
            false,
        ),
        org(
            Role::Admin,
            Some(TokenScopes::new([TokenScope::Admin]).unwrap()),
            true,
        ),
    ]);
    denied
}

async fn call(
    engine: &Engine,
    ctx: &EngineContext,
    request: EngineRequest,
) -> Result<Value, EngineError> {
    knowell_server::Engine::call(engine, ctx, request).await
}

fn forbidden(result: Result<Value, EngineError>) -> EngineError {
    let error = match result {
        Err(error @ EngineError::Forbidden { .. }) => error,
        Err(_) => panic!("native organization operation returned a non-forbidden error"),
        Ok(_) => panic!("native organization operation accepted insufficient authority"),
    };
    let diagnostic = error.to_string();
    assert!(!diagnostic.contains("KNOWELL_CANARY"));
    assert!(!diagnostic.contains(&PROFILE.to_string()));
    assert!(!diagnostic.contains("synthetic-project"));
    assert!(!diagnostic.contains("synthetic-workspace"));
    assert!(!diagnostic.chars().any(char::is_control));
    error
}

async fn snapshot(db: &TestDb) -> Value {
    let mut conn = db.store.acquire().await.unwrap();
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
         'profiles', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM embedding_profile t),
         'jobs', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM job t),
         'views', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM view t),
         'generations', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY to_jsonb(t)::text), '[]'::jsonb) FROM view_generation t),
         'principals', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM principal t),
         'grants', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM access_grant t),
         'audit', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM audit_log t))",
    ).fetch_one(&mut *conn).await.unwrap()
}

fn report_files(fixture: &Fixture) -> Vec<(std::ffi::OsString, Vec<u8>)> {
    let mut files: Vec<_> = std::fs::read_dir(fixture.reports.path())
        .unwrap()
        .map(|entry| {
            let entry = entry.unwrap();
            (entry.file_name(), std::fs::read(entry.path()).unwrap())
        })
        .collect();
    files.sort_by(|a, b| a.0.cmp(&b.0));
    files
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_organization_reads_deny_narrow_grants_and_non_read_credentials() {
    let db = require_db!();
    let fixture = fixture(&db, ServerRole::Standalone).await;
    let before = snapshot(&db).await;
    let files = report_files(&fixture);
    let stats = fixture.engine.indexer().stats();
    for ctx in read_denied() {
        for request in [
            EngineRequest::Profiles,
            EngineRequest::SwitchEstimate {
                to_profile_id: PROFILE.to_string(),
            },
            EngineRequest::EvalReports,
            EngineRequest::Usage { days: 7 },
            EngineRequest::Integrations,
        ] {
            forbidden(call(&fixture.engine, &ctx, request).await);
        }
        for days in [0, 366, u32::MAX] {
            forbidden(call(&fixture.engine, &ctx, EngineRequest::Usage { days }).await);
        }
    }
    assert_eq!(fixture.engine.indexer().stats(), stats);
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(report_files(&fixture), files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_profile_probes_are_forbidden_before_parse_or_closed_store_lookup() {
    let db = require_db!();
    let fixture = fixture(&db, ServerRole::Standalone).await;
    let known = PROFILE.to_string();
    let unknown = Uuid::nil().to_string();
    let oversized = "KNOWELL_CANARY_OVERSIZED_PROFILE".repeat(100);
    let probes = [
        known.as_str(),
        unknown.as_str(),
        "KNOWELL_CANARY_BAD_PROFILE\u{001b}[31m\n",
        oversized.as_str(),
        "",
    ];
    let files = report_files(&fixture);
    db.store.close().await;
    for ctx in read_denied() {
        forbidden(call(&fixture.engine, &ctx, EngineRequest::Profiles).await);
        let mut previous = None;
        for probe in probes {
            let error = forbidden(
                call(
                    &fixture.engine,
                    &ctx,
                    EngineRequest::SwitchEstimate {
                        to_profile_id: probe.to_owned(),
                    },
                )
                .await,
            );
            if let Some(previous) = &previous {
                assert_eq!(
                    &error, previous,
                    "profile existence or input shape changed forbidden response"
                );
            }
            previous = Some(error);
        }
    }
    assert_eq!(report_files(&fixture), files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_start_switch_requires_org_manage_providers_before_parse_or_store() {
    let db = require_db!();
    let fixture = fixture(&db, ServerRole::Standalone).await;
    let known = PROFILE.to_string();
    let unknown = Uuid::nil().to_string();
    let oversized = "KNOWELL_CANARY_OVERSIZED_SWITCH".repeat(100);
    let probes = [
        known.as_str(),
        unknown.as_str(),
        "KNOWELL_CANARY_BAD_SWITCH\u{001b}\n",
        oversized.as_str(),
    ];
    let files = report_files(&fixture);
    let stats = fixture.engine.indexer().stats();
    db.store.close().await;
    for ctx in management_denied() {
        let mut previous = None;
        for probe in probes {
            let error = forbidden(
                call(
                    &fixture.engine,
                    &ctx,
                    EngineRequest::StartSwitch(SwitchRequest {
                        to_profile_id: probe.to_owned(),
                    }),
                )
                .await,
            );
            if let Some(previous) = &previous {
                assert_eq!(
                    &error, previous,
                    "switch input shape changed forbidden response"
                );
            }
            previous = Some(error);
        }
    }
    assert_eq!(fixture.engine.indexer().stats(), stats);
    assert_eq!(report_files(&fixture), files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_human_org_admin_reaches_switch_validation_without_provider_work() {
    let db = require_db!();
    let fixture = fixture(&db, ServerRole::Standalone).await;
    let owner = org(
        Role::Admin,
        Some(TokenScopes::new([TokenScope::Admin]).unwrap()),
        false,
    );
    let before = snapshot(&db).await;
    let files = report_files(&fixture);
    let stats = fixture.engine.indexer().stats();
    let result = call(
        &fixture.engine,
        &owner,
        EngineRequest::StartSwitch(SwitchRequest {
            to_profile_id: PROFILE.to_string(),
        }),
    )
    .await;
    match result {
        Err(EngineError::Invalid { message }) => assert!(message.contains("no embedder")),
        _ => panic!("authorized switch did not report its missing configured embedder"),
    }
    let mut previous = None;
    for probe in [
        Uuid::nil().to_string(),
        "KNOWELL_CANARY_BAD_AUTHORIZED_SWITCH\u{001b}\n".to_owned(),
    ] {
        let error = match call(
            &fixture.engine,
            &owner,
            EngineRequest::StartSwitch(SwitchRequest {
                to_profile_id: probe,
            }),
        )
        .await
        {
            Err(error @ EngineError::NotFound { .. }) => error,
            _ => panic!("authorized absent switch target did not report not found"),
        };
        assert!(!error.to_string().contains("KNOWELL_CANARY"));
        assert!(!error.to_string().chars().any(char::is_control));
        if let Some(previous) = &previous {
            assert_eq!(&error, previous);
        }
        previous = Some(error);
    }
    assert_eq!(fixture.engine.indexer().stats(), stats);
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(report_files(&fixture), files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_admin_requires_org_manage_users_only_on_hub() {
    let db = require_db!();
    let hub = fixture(&db, ServerRole::Hub).await;
    let standalone = fixture(&db, ServerRole::Standalone).await;
    let edge = fixture(&db, ServerRole::Edge).await;
    let owner = org(
        Role::Admin,
        Some(TokenScopes::new([TokenScope::Admin]).unwrap()),
        false,
    );
    let before = snapshot(&db).await;
    let overview = call(&hub.engine, &owner, EngineRequest::Admin)
        .await
        .unwrap();
    assert_eq!(
        overview,
        json!({"available": true, "role": "hub", "users": [], "tokens": [], "audit": []})
    );
    for ctx in management_denied() {
        forbidden(call(&hub.engine, &ctx, EngineRequest::Admin).await);
    }
    assert_eq!(snapshot(&db).await, before);
    db.store.close().await;
    for ctx in management_denied() {
        forbidden(call(&hub.engine, &ctx, EngineRequest::Admin).await);
        for (engine, role) in [(&standalone.engine, "standalone"), (&edge.engine, "edge")] {
            assert_eq!(
                call(engine, &ctx, EngineRequest::Admin).await.unwrap(),
                json!({"available": false, "role": role, "users": [], "tokens": [], "audit": []})
            );
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_usage_validates_authorized_day_bounds_without_panicking() {
    let db = require_db!();
    let fixture = fixture(&db, ServerRole::Standalone).await;
    let viewer = org(Role::Viewer, Some(TokenScopes::read_only()), false);
    let before = snapshot(&db).await;
    let files = report_files(&fixture);
    let mut previous = None;
    for days in [u32::MAX, 0, 366] {
        let error = match call(&fixture.engine, &viewer, EngineRequest::Usage { days }).await {
            Err(error @ EngineError::Invalid { .. }) => error,
            Err(_) => panic!("authorized invalid usage period returned a non-invalid error"),
            Ok(_) => panic!("authorized invalid usage period was accepted"),
        };
        if let Some(previous) = &previous {
            assert_eq!(
                &error, previous,
                "invalid usage period changed static diagnostic"
            );
        }
        previous = Some(error);
    }
    for days in [1, 365] {
        let expected = json!({"periodDays": days, "tools": [], "agents": [], "daily": []});
        assert_eq!(
            call(&fixture.engine, &viewer, EngineRequest::Usage { days })
                .await
                .unwrap(),
            expected
        );
        assert_eq!(
            call(&fixture.engine, &viewer, EngineRequest::Usage { days })
                .await
                .unwrap(),
            expected
        );
    }
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(report_files(&fixture), files);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn native_org_viewer_retains_profiles_estimates_and_evaluation_outputs() {
    let db = require_db!();
    let fixture = fixture(&db, ServerRole::Standalone).await;
    let viewer = org(Role::Viewer, Some(TokenScopes::read_only()), false);
    let before = snapshot(&db).await;
    let files = report_files(&fixture);
    let profiles = call(&fixture.engine, &viewer, EngineRequest::Profiles)
        .await
        .unwrap();
    assert_eq!(profiles.as_array().unwrap().len(), 1);
    assert_eq!(profiles[0]["id"], PROFILE.to_string());
    assert_eq!(profiles[0]["name"], "synthetic-rest-profile");
    assert_eq!(profiles[0]["provider"], "synthetic-unavailable-provider");
    assert_eq!(profiles[0]["dimensions"], 64);
    let estimate = call(
        &fixture.engine,
        &viewer,
        EngineRequest::SwitchEstimate {
            to_profile_id: PROFILE.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(estimate["toProfileId"], PROFILE.to_string());
    assert_eq!(estimate["affectedProjects"], json!([]));
    assert_eq!(estimate["chunksToRegenerate"], 0);
    assert_eq!(estimate["estimatedTokens"], 0);
    assert!(
        estimate["warnings"]
            .as_array()
            .unwrap()
            .iter()
            .any(|warning| warning.as_str().unwrap().contains("no embedder"))
    );
    let reports = call(&fixture.engine, &viewer, EngineRequest::EvalReports)
        .await
        .unwrap();
    let saved = knowell_eval::Report::from_json(REPORT).unwrap();
    assert_eq!(reports.as_array().unwrap().len(), 1);
    assert_eq!(reports[0]["id"], "synthetic-permissions");
    assert_eq!(reports[0]["querySet"], saved.query_set.fixture);
    assert_eq!(reports[0]["queryCount"], saved.query_set.queries);
    assert!(reports[0]["slices"].as_array().unwrap().len() >= saved.retrievers.len());
    let integrations = call(&fixture.engine, &viewer, EngineRequest::Integrations)
        .await
        .unwrap();
    assert_eq!(
        integrations,
        json!({"mcp": {"transport": "stdio", "endpoint": "stdio", "status": "idle"}, "agents": [], "webhooks": []})
    );
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(report_files(&fixture), files);
}
