//! Saved memory evidence retains its source version as the index catches up.

use std::sync::Arc;

use knowell_engine::Engine;
use knowell_index::{Priority, SyncOutcome};
use knowell_mcp::tools::{
    InspectSymbolInput, MemoryKind, MemoryScope, OpenWorkspaceInput, ReadMemoryInput, ScopeLevel,
    SearchInput, SearchKind, WriteMemoryInput,
};
use knowell_mcp::{Caller, Evidence, IndexState, KnowellTools, MemoryId, SymbolRef, Target};
use knowell_store::{ViewId, views};

use crate::common::{
    access, alice_caller, fixture_workspace, git_available, indexer_config, name, require_db,
};

const PROJECT: &str = "billing-api";
const QUERY: &str = "memfreshnessquokka";

async fn assert_memory_state(
    engine: &Engine,
    caller: &Caller,
    view: ViewId,
    id: &MemoryId,
    saved: &Evidence,
    target: Target,
    state: IndexState,
) {
    let stats = engine.indexer().stats();
    let mut conn = engine.store().acquire().await.unwrap();
    let before = views::get_view(&mut conn, view).await.unwrap().unwrap();
    let building = views::building_generation(&mut conn, view).await.unwrap();
    let jobs: serde_json::Value = sqlx::query_scalar(
        "SELECT coalesce(jsonb_agg(to_jsonb(job) ORDER BY id), '[]'::jsonb) FROM job",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    drop(conn);

    let mut expected = saved.clone();
    expected.index_state = state;
    let read = engine
        .read_memory(
            caller,
            ReadMemoryInput {
                target: target.clone(),
                ids: vec![id.clone()],
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(read.records.len(), 1);
    assert_eq!(&read.records[0].id, id);
    assert_eq!(read.records[0].evidence, vec![expected.clone()]);

    let found = engine
        .search(
            caller,
            SearchInput {
                target,
                query: QUERY.to_owned(),
                projects: vec![name(PROJECT)],
                kinds: vec![SearchKind::Memory],
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(found.hits.is_empty());
    assert_eq!(found.memory_hits.len(), 1);
    assert_eq!(&found.memory_hits[0].record.id, id);
    assert_eq!(found.memory_hits[0].record.evidence, vec![expected]);
    assert!(!found.memory_hits[0].why.is_empty());

    let mut conn = engine.store().acquire().await.unwrap();
    let after = views::get_view(&mut conn, view).await.unwrap().unwrap();
    assert_eq!(after.active_generation, before.active_generation);
    assert_eq!(after.active_commit, before.active_commit);
    assert_eq!(after.latest_seen_commit, before.latest_seen_commit);
    assert_eq!(after.last_generation, before.last_generation);
    assert_eq!(
        views::building_generation(&mut conn, view).await.unwrap(),
        building
    );
    let after_jobs: serde_json::Value = sqlx::query_scalar(
        "SELECT coalesce(jsonb_agg(to_jsonb(job) ORDER BY id), '[]'::jsonb) FROM job",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(after_jobs, jobs);
    assert_eq!(engine.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persisted_memory_reports_source_and_build_freshness_without_repinning_evidence() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == PROJECT);
    for project in &mut ws.resolved.projects {
        project.embedding.provider = None;
        project.embedding.model = None;
    }
    let data = tempfile::tempdir().unwrap();
    let original = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .workspace(ws.resolved.clone())
        .access(Arc::new(access()))
        .build()
        .await
        .unwrap();
    let (_, outcomes) = original
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, SyncOutcome::Failed { .. }))
    );
    let caller = alice_caller();
    let inspected = original
        .inspect_symbol(
            &caller,
            InspectSymbolInput {
                target: Target::default(),
                symbol: SymbolRef {
                    id: None,
                    symbol: Some("SubscriptionService.cancelSubscription".to_owned()),
                    project: Some(name(PROJECT)),
                },
                include: Vec::new(),
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(inspected.symbols.len(), 1);
    let written = original
        .write_memory(
            &caller,
            WriteMemoryInput {
                target: Target::default(),
                scope: MemoryScope {
                    level: ScopeLevel::Project,
                    project: Some(name(PROJECT)),
                    task_id: None,
                },
                kind: MemoryKind::Decision,
                title: format!("{QUERY} saved source decision"),
                body: format!("{QUERY} cites the synthetic cancellation implementation."),
                related_symbols: Vec::new(),
                evidence: vec![inspected.symbols[0].id.clone()],
                supersedes: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap();
    assert!(written.created);
    assert_eq!(written.record.evidence.len(), 1);
    let saved = written.record.evidence[0].clone();
    assert_eq!(saved.index_state, IndexState::Current);
    assert_eq!(saved.project, name(PROJECT));
    assert_eq!(
        saved.path.as_str(),
        "src/subscriptions/subscription.service.ts"
    );
    assert_eq!(
        saved.commit.as_str(),
        ws.git(PROJECT, &["rev-parse", "HEAD"])
    );
    drop(original);

    // A fresh engine must load this record and its evidence from PostgreSQL.
    let engine = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .workspace(ws.resolved.clone())
        .access(Arc::new(access()))
        .build()
        .await
        .unwrap();
    let registration = engine.add_workspace(&ws.resolved).await.unwrap();
    assert!(registration.issues.is_empty());
    assert_eq!(registration.views.len(), 1);
    let view = registration.views[0].view;
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let old_context = Target::context(opened.context_id);
    let target = Target::workspace(ws.resolved.name.clone(), Vec::new());
    assert_memory_state(
        &engine,
        &caller,
        view,
        &written.record.id,
        &saved,
        target.clone(),
        IndexState::Current,
    )
    .await;

    std::fs::write(
        ws.project_dir(PROJECT).join("source-version.txt"),
        "synthetic branch advancement without indexing\n",
    )
    .unwrap();
    let head = ws.commit_all(PROJECT, "advance the saved memory fixture branch");
    assert_ne!(head, saved.commit.as_str());
    assert_memory_state(
        &engine,
        &caller,
        view,
        &written.record.id,
        &saved,
        target.clone(),
        IndexState::Stale,
    )
    .await;
    let status = engine.indexer().status(view).await.unwrap();
    assert_eq!(status.active_commit.as_deref(), Some(saved.commit.as_str()));
    assert_eq!(
        status.latest_seen_commit.as_deref(),
        Some(saved.commit.as_str())
    );
    assert!(status.building_generation.is_none());

    let queued = engine
        .indexer()
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    assert!(matches!(queued, SyncOutcome::Queued { .. }));
    // Complete only T0; T1 and relation work remain queued while the old
    // active generation continues serving both memory APIs.
    assert!(engine.indexer().run_next_job().await.unwrap());
    let building = engine.indexer().status(view).await.unwrap();
    assert_eq!(
        building.active_commit.as_deref(),
        Some(saved.commit.as_str())
    );
    assert_eq!(building.latest_seen_commit.as_deref(), Some(head.as_str()));
    assert!(building.building_generation.is_some());
    assert_memory_state(
        &engine,
        &caller,
        view,
        &written.record.id,
        &saved,
        target.clone(),
        IndexState::CatchingUp,
    )
    .await;
    assert_memory_state(
        &engine,
        &caller,
        view,
        &written.record.id,
        &saved,
        old_context.clone(),
        IndexState::Current,
    )
    .await;

    let run = engine.indexer().run_until_idle().await.unwrap();
    assert_eq!(run.failed, 0);
    let active = engine.indexer().status(view).await.unwrap();
    assert_eq!(active.active_commit.as_deref(), Some(head.as_str()));
    assert!(active.building_generation.is_none());
    assert_memory_state(
        &engine,
        &caller,
        view,
        &written.record.id,
        &saved,
        target,
        IndexState::Stale,
    )
    .await;
    assert_memory_state(
        &engine,
        &caller,
        view,
        &written.record.id,
        &saved,
        old_context,
        IndexState::Current,
    )
    .await;
}
