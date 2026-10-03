//! Stored input formats are part of profile identity before a native switch mutates state.

use std::collections::BTreeSet;
use std::sync::Arc;

use knowell_auth::{Grant, GrantSet, Principal, RequestId, ResourceScope, Role, visible_projects};
use knowell_config::{Origin, ResolvedWorkspace, Sourced, parse_engine};
use knowell_embed::{AnyEmbedder, FAKE_MODEL, FakeEmbedder, INPUT_FORMAT_VERSION};
use knowell_engine::Engine;
use knowell_index::{EmbeddingPlan, Priority, Registration, SyncOutcome};
use knowell_mcp::tools::{OpenWorkspaceInput, SearchInput};
use knowell_mcp::{KnowellTools, MatchReason, Target};
use knowell_server::{EngineContext, EngineError, EngineRequest, MemoryAuditSink, SwitchRequest};
use knowell_store::embeddings::{self, EmbeddingProfile, NewEmbeddingProfile};
use knowell_store::{ProfileId, ViewId};
use serde_json::{Value, json};
use uuid::Uuid;

use crate::common::{
    TestDb, Workspace, access, alice, alice_caller, fixture_workspace, git_available,
    indexer_config, name, require_db,
};

struct Fixture {
    engine: Engine,
    view: ViewId,
    a: EmbeddingProfile,
    b: EmbeddingProfile,
    /// The configuration the view was indexed with.
    active: ResolvedWorkspace,
    workspace: Workspace,
    data: tempfile::TempDir,
}

/// The engine with providers `a` (64 dimensions) and `b` (96), as a fresh
/// process would build it.
async fn build_engine(db: &TestDb, data: &std::path::Path) -> Engine {
    let config = parse_engine(&format!(
        "version = 1\n[providers.a]\nkind = \"ollama\"\nmodel = \"{FAKE_MODEL}\"\n[providers.b]\nkind = \"ollama\"\nmodel = \"{FAKE_MODEL}\"\n"
    ))
    .unwrap();
    Engine::builder(db.store.clone(), indexer_config(data))
        .engine_config(&config)
        .embedder(
            name("a"),
            Arc::new(AnyEmbedder::Fake(FakeEmbedder::new(64).unwrap())),
        )
        .embedder(
            name("b"),
            Arc::new(AnyEmbedder::Fake(FakeEmbedder::new(96).unwrap())),
        )
        .access(Arc::new(access()))
        .build()
        .await
        .unwrap()
}

fn configured(workspace: &ResolvedWorkspace, provider: &str, dimensions: u32) -> ResolvedWorkspace {
    let mut resolved = workspace.clone();
    for project in &mut resolved.projects {
        project.embedding.provider = Some(Sourced {
            value: name(provider),
            origin: Origin::Project,
        });
        project.embedding.dimensions = Sourced {
            value: dimensions,
            origin: Origin::Project,
        };
    }
    resolved
}

fn registered_profile(registration: &Registration) -> ProfileId {
    assert!(
        registration.issues.is_empty(),
        "synthetic registration failed"
    );
    assert_eq!(registration.views.len(), 1);
    match &registration.views[0].embedding {
        EmbeddingPlan::Embed { profile, .. } => *profile,
        _ => panic!("synthetic provider did not register its profile"),
    }
}

async fn stored_profile(db: &TestDb, profile: ProfileId) -> EmbeddingProfile {
    let mut conn = db.store.acquire().await.unwrap();
    embeddings::get_profile(&mut conn, profile)
        .await
        .unwrap()
        .unwrap()
}

async fn fixture(db: &TestDb, active_b: bool) -> Fixture {
    let mut workspace = fixture_workspace();
    workspace
        .resolved
        .projects
        .retain(|p| p.name == name("billing-api"));
    assert_eq!(workspace.resolved.projects.len(), 1);
    let a_workspace = configured(&workspace.resolved, "a", 64);
    let b_workspace = configured(&workspace.resolved, "b", 96);
    let data = tempfile::tempdir().unwrap();
    let engine = build_engine(db, data.path()).await;

    // Registration supplies the canonical embed/prepared/parser compound format;
    // the test never invents the supported store profile's version string.
    let b_registration = engine.add_workspace(&b_workspace).await.unwrap();
    let b = stored_profile(db, registered_profile(&b_registration)).await;
    let a_registration = engine.add_workspace(&a_workspace).await.unwrap();
    let a = stored_profile(db, registered_profile(&a_registration)).await;
    assert_ne!(a.id, b.id);
    assert_eq!(a.input_format_version, b.input_format_version);
    let active = if active_b { &b_workspace } else { &a_workspace };
    let registration = engine.add_workspace(active).await.unwrap();
    let view = registration.views[0].view;
    let (_, outcomes) = engine
        .indexer()
        .index_workspace(active, Priority::Interactive)
        .await
        .unwrap();
    assert_eq!(outcomes.len(), 1);
    assert!(matches!(outcomes[0], SyncOutcome::Queued { .. }));
    let coverage = engine
        .indexer()
        .embedding_coverage(view)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(coverage.profile, if active_b { b.id } else { a.id });
    assert!(coverage.inputs > 0);
    assert_eq!(coverage.embedded, coverage.inputs);
    assert!(coverage.complete);
    let active = active.clone();
    Fixture {
        engine,
        view,
        a,
        b,
        active,
        workspace,
        data,
    }
}

fn owner() -> EngineContext {
    let principal = Principal::User(alice());
    let mut grants = GrantSet::new();
    grants.add(Grant::new(principal.clone(), Role::Admin, ResourceScope::Organization).unwrap());
    EngineContext {
        visible: visible_projects(&principal, &grants),
        principal,
        grants: Arc::new(grants),
        scopes: None,
        request_id: RequestId::new("synthetic-profile-switch").unwrap(),
        audit: Arc::new(MemoryAuditSink::new()),
    }
}

async fn call(
    engine: &Engine,
    ctx: &EngineContext,
    request: EngineRequest,
) -> Result<Value, EngineError> {
    knowell_server::Engine::call(engine, ctx, request).await
}

fn unsupported_formats(canonical: &str) -> Vec<(&'static str, String)> {
    let prepared = knowell_parse::PREPARED_FORMAT_VERSION;
    let parser = knowell_parse::PARSER_VERSION;
    vec![
        (
            "older-embed",
            format!(
                "embed{}-prepared{prepared}-parser{parser}",
                INPUT_FORMAT_VERSION.saturating_sub(1)
            ),
        ),
        (
            "older-prepared",
            format!(
                "embed{INPUT_FORMAT_VERSION}-prepared{}-parser{parser}",
                prepared.saturating_sub(1)
            ),
        ),
        (
            "older-parser",
            format!(
                "embed{INPUT_FORMAT_VERSION}-prepared{prepared}-parser{}",
                parser.saturating_sub(1)
            ),
        ),
        ("malformed", "synthetic-unrecognized-format".to_owned()),
        (
            "hostile",
            format!("{canonical}-KNOWELL_CANARY_FAKE_FORMAT\u{001b}[31m\n"),
        ),
    ]
}

async fn unsupported_profiles(db: &TestDb, original: &EmbeddingProfile) -> Vec<EmbeddingProfile> {
    let mut profiles = Vec::new();
    for (label, input_format_version) in unsupported_formats(&original.input_format_version) {
        assert_ne!(input_format_version, original.input_format_version);
        let mut conn = db.store.acquire().await.unwrap();
        let stored = embeddings::register_profile(
            &mut conn,
            original.organization,
            &NewEmbeddingProfile {
                name: name(&format!("synthetic-{label}-{}", original.dimensions)),
                provider: original.provider.clone(),
                model: original.model.clone(),
                dimensions: original.dimensions,
                input_format_version,
            },
        )
        .await
        .unwrap();
        profiles.push(stored);
    }
    profiles
}

async fn snapshot(db: &TestDb) -> Value {
    let mut conn = db.store.acquire().await.unwrap();
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
         'profiles', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM embedding_profile t),
         'jobs', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM job t),
         'views', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM view t),
         'generations', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY view_id, generation), '[]'::jsonb) FROM view_generation t),
         'index_generations', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM index_generation t),
         'embeddings', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY profile_id, prepared_input_hash), '[]'::jsonb) FROM embedding t))",
    ).fetch_one(&mut *conn).await.unwrap()
}

async fn runtime(fixture: &Fixture, ctx: &EngineContext) -> Value {
    json!({
        "profiles": call(&fixture.engine, ctx, EngineRequest::Profiles).await.unwrap(),
        "status": fixture.engine.indexer().status(fixture.view).await.unwrap(),
        "coverage": fixture.engine.indexer().embedding_coverage(fixture.view).await.unwrap(),
    })
}

fn cannot_start(estimate: &Value) -> bool {
    estimate["warnings"]
        .as_array()
        .unwrap()
        .iter()
        .any(|warning| {
            warning.as_str().is_some_and(|message| {
                message.contains("no embedder") && message.contains("cannot start")
            })
        })
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unsupported_input_formats_fail_before_profile_switch_mutation() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let fixture = fixture(&db, false).await;
    let ctx = owner();
    let unsupported = unsupported_profiles(&db, &fixture.b).await;
    let before = snapshot(&db).await;
    let active = runtime(&fixture, &ctx).await;
    let stats = fixture.engine.indexer().stats();
    let mut prior_error = None;
    for profile in unsupported {
        let error = match call(
            &fixture.engine,
            &ctx,
            EngineRequest::StartSwitch(SwitchRequest {
                to_profile_id: profile.id.to_string(),
            }),
        )
        .await
        {
            Err(error @ EngineError::Invalid { .. }) => error,
            Err(_) => panic!("unsupported input format returned a non-invalid error"),
            Ok(_) => panic!("unsupported input format accepted a profile switch"),
        };
        let diagnostic = error.to_string();
        assert!(!diagnostic.contains(&profile.input_format_version));
        assert!(!diagnostic.contains("KNOWELL_CANARY"));
        assert!(!diagnostic.chars().any(char::is_control));
        if let Some(previous) = &prior_error {
            assert_eq!(&error, previous);
        }
        prior_error = Some(error);
        assert_eq!(snapshot(&db).await, before);
        assert_eq!(runtime(&fixture, &ctx).await, active);
        assert_eq!(fixture.engine.indexer().stats(), stats);
    }

    // A supported target still starts actual work. No worker is run here and
    // no assertion claims that the target's vectors already cover the old view.
    let queued = call(
        &fixture.engine,
        &ctx,
        EngineRequest::StartSwitch(SwitchRequest {
            to_profile_id: fixture.b.id.to_string(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(queued["toProfileId"], fixture.b.id.to_string());
    assert_eq!(queued["state"], "building");
    assert_eq!(queued["views"], json!([fixture.view.to_string()]));
    let jobs: Vec<Uuid> = queued["jobs"]
        .as_array()
        .unwrap()
        .iter()
        .map(|id| Uuid::parse_str(id.as_str().unwrap()).unwrap())
        .collect();
    assert_eq!(jobs.len(), 1);
    let mut conn = db.store.acquire().await.unwrap();
    let persisted: i64 = sqlx::query_scalar("SELECT count(*) FROM job WHERE id = ANY($1)")
        .bind(&jobs)
        .fetch_one(&mut *conn)
        .await
        .unwrap();
    assert_eq!(persisted, 1);
    drop(conn);
    let after = snapshot(&db).await;
    assert_eq!(after["profiles"], before["profiles"]);
    assert_eq!(after["embeddings"], before["embeddings"]);
    assert_eq!(after["index_generations"], before["index_generations"]);
    let status = fixture.engine.indexer().status(fixture.view).await.unwrap();
    assert_eq!(
        json!(status.active_generation),
        active["status"]["active_generation"]
    );
    assert_eq!(
        json!(status.active_commit),
        active["status"]["active_commit"]
    );
    // The old profile keeps serving, complete, until the target covers the
    // view; the stored switch reports the target's progress.
    let coverage = fixture
        .engine
        .indexer()
        .embedding_coverage(fixture.view)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(coverage.profile, fixture.a.id);
    assert!(coverage.complete);
    assert_eq!(coverage.embedded, coverage.inputs);
    let switches = call(&fixture.engine, &ctx, EngineRequest::Switches)
        .await
        .unwrap();
    // Newest first: this switch, then the one the fixture's configuration
    // change (from b to a) started while nothing was indexed yet.
    let switches = switches.as_array().unwrap();
    assert_eq!(switches.len(), 2);
    assert_eq!(switches[1]["origin"], "configuration");
    let ours = &switches[0];
    assert_eq!(ours["id"], queued["switchId"]);
    assert_eq!(ours["origin"], "request");
    assert_eq!(ours["state"], "building");
    assert_eq!(ours["fromProfileId"], fixture.a.id.to_string());
    assert_eq!(ours["toProfileId"], fixture.b.id.to_string());
    assert_eq!(ours["progress"], 0.0);
    assert_eq!(ours["views"][0]["covered"], false);
    assert_eq!(ours["views"][0]["embedded"], 0);
    assert!(ours["views"][0]["inputs"].as_u64().unwrap() > 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn estimates_require_matching_input_formats_even_for_dimension_reduction() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let fixture = fixture(&db, true).await;
    let ctx = owner();
    let unsupported = unsupported_profiles(&db, &fixture.a).await;
    let before = snapshot(&db).await;
    let active = runtime(&fixture, &ctx).await;
    let stats = fixture.engine.indexer().stats();
    let supported = call(
        &fixture.engine,
        &ctx,
        EngineRequest::SwitchEstimate {
            to_profile_id: fixture.a.id.to_string(),
        },
    )
    .await
    .unwrap();
    assert_eq!(supported["fromProfileId"], fixture.b.id.to_string());
    assert_eq!(supported["toProfileId"], fixture.a.id.to_string());
    assert_eq!(supported["affectedProjects"], json!(["billing-api"]));
    // No vectors are derived from another profile: a target without
    // vectors for the generation is embedded again, also when it only has
    // fewer dimensions.
    assert!(supported["chunksToRegenerate"].as_u64().unwrap() > 0);
    assert_eq!(supported["needsReembedding"], true);
    assert!(supported["estimatedTokens"].as_u64().unwrap() > 0);
    assert!(!cannot_start(&supported));
    for profile in unsupported {
        assert_eq!(profile.provider, fixture.a.provider);
        assert_eq!(profile.model, fixture.a.model);
        assert_eq!(profile.dimensions, fixture.a.dimensions);
        let estimate = call(
            &fixture.engine,
            &ctx,
            EngineRequest::SwitchEstimate {
                to_profile_id: profile.id.to_string(),
            },
        )
        .await
        .unwrap();
        assert_eq!(estimate["fromProfileId"], fixture.b.id.to_string());
        assert_eq!(estimate["toProfileId"], profile.id.to_string());
        assert_eq!(estimate["affectedProjects"], supported["affectedProjects"]);
        assert_eq!(
            estimate["chunksToRegenerate"],
            supported["chunksToRegenerate"]
        );
        assert_eq!(
            estimate["needsReembedding"], true,
            "different input format was treated as reusable"
        );
        assert!(estimate["estimatedTokens"].as_u64().unwrap() > 0);
        assert!(
            cannot_start(&estimate),
            "unsupported profile lacked an actionable warning"
        );
        assert!(!estimate.to_string().contains("KNOWELL_CANARY"));
    }
    assert_eq!(snapshot(&db).await, before);
    assert_eq!(runtime(&fixture, &ctx).await, active);
    assert_eq!(fixture.engine.indexer().stats(), stats);
}

/// Embedding profile names of the semantic matches of a search.
async fn semantic_profiles(engine: &Engine, target: Target) -> BTreeSet<String> {
    let found = engine
        .search(
            &alice_caller(),
            SearchInput {
                target,
                query: "how is a customer subscription cancelled".to_owned(),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    found
        .hits
        .iter()
        .flat_map(|hit| hit.evidence.why.iter())
        .filter_map(|why| match why {
            MatchReason::Semantic { profile, .. } => Some(profile.clone()),
            _ => None,
        })
        .collect()
}

fn workspace_target(fixture: &Fixture) -> Target {
    Target::workspace(fixture.active.name.clone(), Vec::new())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switches_serve_one_profile_at_a_time_survive_restarts_and_roll_back() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let fixture = fixture(&db, false).await;
    let ctx = owner();
    let (a, b) = (fixture.a.name.to_string(), fixture.b.name.to_string());
    let only = |name: &str| BTreeSet::from([name.to_owned()]);
    let opened = fixture
        .engine
        .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
        .await
        .unwrap();
    let old_context = Target::context(opened.context_id);
    assert_eq!(
        semantic_profiles(&fixture.engine, workspace_target(&fixture)).await,
        only(&a)
    );

    // Start: the old profile keeps answering until the target is built.
    let started = call(
        &fixture.engine,
        &ctx,
        EngineRequest::StartSwitch(SwitchRequest {
            to_profile_id: fixture.b.id.to_string(),
        }),
    )
    .await
    .unwrap();
    let switch_id = started["switchId"].as_str().unwrap().to_owned();
    assert_eq!(
        semantic_profiles(&fixture.engine, workspace_target(&fixture)).await,
        only(&a)
    );
    fixture.engine.indexer().run_until_idle().await.unwrap();

    // Flipped: new searches use the target; a context opened before the
    // switch keeps the profile it was opened with.
    assert_eq!(
        semantic_profiles(&fixture.engine, workspace_target(&fixture)).await,
        only(&b)
    );
    assert_eq!(
        semantic_profiles(&fixture.engine, old_context.clone()).await,
        only(&a)
    );
    let listed = call(&fixture.engine, &ctx, EngineRequest::Switches)
        .await
        .unwrap();
    assert_eq!(listed[0]["state"], "active");
    assert_eq!(listed[0]["progress"], 1.0);
    assert!(listed[0]["reversibleUntil"].is_string());

    // Roll back: the old vectors still cover the generation, so it is
    // immediate and sends nothing to a provider.
    let calls = fixture.engine.indexer().stats().embedding_calls;
    let reverse = call(
        &fixture.engine,
        &ctx,
        EngineRequest::RollbackSwitch {
            switch_id: switch_id.clone(),
        },
    )
    .await
    .unwrap();
    assert_eq!(reverse["state"], "active");
    assert_eq!(reverse["rollbackOf"], switch_id.as_str());
    assert_eq!(fixture.engine.indexer().stats().embedding_calls, calls);
    assert_eq!(
        semantic_profiles(&fixture.engine, workspace_target(&fixture)).await,
        only(&a)
    );
    let listed = call(&fixture.engine, &ctx, EngineRequest::Switches)
        .await
        .unwrap();
    assert_eq!(listed[1]["id"], switch_id.as_str());
    assert_eq!(listed[1]["state"], "rolled-back");
    // A switch that ended cannot be rolled back again or cancelled.
    for request in [
        EngineRequest::RollbackSwitch {
            switch_id: switch_id.clone(),
        },
        EngineRequest::CancelSwitch {
            switch_id: switch_id.clone(),
        },
    ] {
        assert!(matches!(
            call(&fixture.engine, &ctx, request).await,
            Err(EngineError::Invalid { .. })
        ));
    }

    // A commit, then a switch the process does not finish: a fresh engine
    // resumes it from the store and flips.
    let note = fixture
        .workspace
        .project_dir("billing-api")
        .join("switch-note.md");
    std::fs::write(
        &note,
        "# Switch note\n\nSynthetic text added before a restart.\n",
    )
    .unwrap();
    fixture
        .workspace
        .commit_all("billing-api", "add a note before the restart");
    fixture
        .engine
        .indexer()
        .index_workspace(&fixture.active, Priority::Interactive)
        .await
        .unwrap();
    let pending = call(
        &fixture.engine,
        &ctx,
        EngineRequest::StartSwitch(SwitchRequest {
            to_profile_id: fixture.b.id.to_string(),
        }),
    )
    .await
    .unwrap();
    assert_eq!(pending["state"], "building");
    assert_eq!(pending["jobs"].as_array().unwrap().len(), 1);
    let Fixture {
        engine,
        active,
        data,
        workspace: _workspace,
        ..
    } = fixture;
    drop(engine);
    let restarted = build_engine(&db, data.path()).await;
    restarted.add_workspace(&active).await.unwrap();
    restarted
        .indexer()
        .index_workspace(&active, Priority::Interactive)
        .await
        .unwrap();
    assert_eq!(
        semantic_profiles(
            &restarted,
            Target::workspace(active.name.clone(), Vec::new())
        )
        .await,
        only(&b)
    );
    let listed = call(&restarted, &ctx, EngineRequest::Switches)
        .await
        .unwrap();
    assert_eq!(listed[0]["id"], pending["switchId"]);
    assert_eq!(listed[0]["state"], "active");
    // Unknown and malformed switch ids are not found.
    for id in [Uuid::now_v7().to_string(), "not-a-switch".to_owned()] {
        assert!(matches!(
            call(
                &restarted,
                &ctx,
                EngineRequest::CancelSwitch { switch_id: id }
            )
            .await,
            Err(EngineError::NotFound { .. })
        ));
    }
}
