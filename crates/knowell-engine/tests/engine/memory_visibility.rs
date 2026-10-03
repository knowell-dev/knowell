//! Persisted memory and task references obey the caller's current source grants.

use std::path::Path;
use std::sync::{Arc, RwLock};

use knowell_auth::{Grant, GrantSet, Principal, ResourceScope, Role};
use knowell_core::RepoPath;
use knowell_engine::{
    Access, AccessResolver, BoxFuture, Engine, InMemoryMemory, MemoryRepo, StaticAccess,
};
use knowell_index::{Priority, SyncOutcome};
use knowell_mcp::tools::{
    DecisionInput, HistoryFacet, HistoryInput, InspectSymbolInput, MemoryKind, MemoryRecord,
    MemoryScope, MemoryStatus, OpenWorkspaceInput, ReadMemoryInput, ResumeTaskInput,
    SaveCheckpointInput, ScopeLevel, SearchInput, SearchKind, WriteMemoryInput,
};
use knowell_mcp::{
    Caller, Evidence, Gap, GapReason, IndexState, KnowellTools, MemoryId, ResultId, SymbolRef,
    Target, ToolError, ViewLayer,
};
use knowell_server::{EngineContext, EngineRequest, MemoryAction, MemoryDecision};
use knowell_store::knowledge::{self, HistoryEntry, RecordContent, RecordUpdate};
use knowell_store::tasks::{self, NewCheckpoint, NewTask, TaskDetails, TaskUpdate};
use knowell_store::{
    KnowledgeAction, KnowledgeRecordId, KnowledgeState, TaskId, TaskStatus as StoredTaskStatus,
};
use serde::Serialize;
use time::OffsetDateTime;

use crate::common::{
    TestDb, Workspace, access, alice, alice_caller, fixture_workspace, git_available,
    indexer_config, name, require_db,
};

const VISIBLE: &str = "billing-api";
const HIDDEN: &str = "storefront-web";
const ROOT: &str = "memory-fixture";
const VISIBLE_PATH: &str = "src/visible-memory.ts";
const HIDDEN_PATH: &str = "src/hidden-memory.ts";
const QUERY: &str = "memoryvisibilityquokka";
const OMITTED: &str = "some saved references are unavailable in this context";

/// Grants are read again for a cached context, without rebuilding the engine.
struct CurrentAccess(RwLock<StaticAccess>);

impl CurrentAccess {
    fn new() -> Self {
        Self(RwLock::new(access()))
    }

    fn restrict(&self, workspace: &str, project: &str) {
        *self.0.write().unwrap() = project_access(workspace, project);
    }
}

impl AccessResolver for CurrentAccess {
    fn resolve<'a>(&'a self, caller: &'a Caller) -> BoxFuture<'a, Result<Access, ToolError>> {
        let snapshot = self.0.read().unwrap().clone();
        Box::pin(async move { snapshot.resolve(caller).await })
    }
}

fn project_access(workspace: &str, project: &str) -> StaticAccess {
    let mut grants = GrantSet::new();
    grants.add(
        Grant::new(
            Principal::User(alice()),
            Role::Member,
            ResourceScope::project(name(workspace), name(project)),
        )
        .unwrap(),
    );
    StaticAccess::local_user(alice(), grants)
}

fn write_source(ws: &Workspace, project: &str, path: &str, symbol: &str) {
    let destination = ws.project_dir(project).join(ROOT).join(path);
    std::fs::create_dir_all(destination.parent().unwrap()).unwrap();
    std::fs::write(
        destination,
        format!("export function {symbol}(value: number): number {{\n  return value + 1;\n}}\n"),
    )
    .unwrap();
}

fn workspace() -> Workspace {
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| [VISIBLE, HIDDEN].contains(&project.name.as_str()));
    for project in &mut ws.resolved.projects {
        project.root = Some(RepoPath::new(ROOT).unwrap());
        project.embedding.provider = None;
        project.embedding.model = None;
    }
    write_source(&ws, VISIBLE, VISIBLE_PATH, "VisibleMemoryProbe");
    write_source(&ws, HIDDEN, HIDDEN_PATH, "HiddenMemoryProbe");
    for project in [VISIBLE, HIDDEN] {
        ws.commit_all(project, "add synthetic memory source evidence");
    }
    ws
}

async fn engine(
    db: &TestDb,
    ws: &Workspace,
    data: &Path,
    access: Arc<dyn AccessResolver>,
) -> Engine {
    Engine::builder(db.store.clone(), indexer_config(data))
        .workspace(ws.resolved.clone())
        .access(access)
        .build()
        .await
        .unwrap()
}

async fn index(engine: &Engine, ws: &Workspace) {
    let (_, outcomes) = engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, SyncOutcome::Failed { .. }))
    );
}

fn target(ws: &Workspace) -> Target {
    Target::workspace(ws.resolved.name.clone(), Vec::new())
}

async fn source_id(engine: &Engine, ws: &Workspace, project: &str, symbol: &str) -> ResultId {
    let inspected = engine
        .inspect_symbol(
            &alice_caller(),
            InspectSymbolInput {
                target: target(ws),
                symbol: SymbolRef {
                    id: None,
                    symbol: Some(symbol.to_owned()),
                    project: Some(name(project)),
                },
                include: Vec::new(),
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(inspected.symbols.len(), 1);
    inspected.symbols[0].id.clone()
}

async fn write_record(
    engine: &Engine,
    ws: &Workspace,
    scope: ScopeLevel,
    project: Option<&str>,
    label: &str,
    evidence: Vec<ResultId>,
) -> MemoryRecord {
    engine
        .write_memory(
            &alice_caller(),
            WriteMemoryInput {
                target: target(ws),
                scope: MemoryScope {
                    level: scope,
                    project: project.map(name),
                    task_id: None,
                },
                kind: MemoryKind::Decision,
                title: format!("{QUERY} {label}"),
                body: format!("{QUERY} {label} is a synthetic saved decision."),
                related_symbols: Vec::new(),
                evidence,
                supersedes: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap()
        .record
}

/// These rows prove that even a new request-level source pin does not enqueue
/// jobs, change an index generation, or rewrite the persisted memory receipt.
async fn snapshot(engine: &Engine) -> serde_json::Value {
    let mut conn = engine.store().acquire().await.unwrap();
    sqlx::query_scalar(
        "SELECT jsonb_build_object(
           'jobs', (SELECT coalesce(jsonb_agg(to_jsonb(j) ORDER BY id), '[]'::jsonb) FROM job j),
           'views', (SELECT coalesce(jsonb_agg(to_jsonb(v) ORDER BY id), '[]'::jsonb) FROM view v),
           'generations', (SELECT coalesce(jsonb_agg(to_jsonb(g) ORDER BY view_id, generation), '[]'::jsonb) FROM view_generation g),
           'records', (SELECT coalesce(jsonb_agg(to_jsonb(r) ORDER BY id), '[]'::jsonb) FROM knowledge_record r),
           'evidence', (SELECT coalesce(jsonb_agg(to_jsonb(e) ORDER BY record_id, version, ordinal), '[]'::jsonb) FROM knowledge_evidence e),
           'tasks', (SELECT coalesce(jsonb_agg(to_jsonb(t) ORDER BY id), '[]'::jsonb) FROM task t),
           'checkpoints', (SELECT coalesce(jsonb_agg(to_jsonb(c) ORDER BY task_id, seq), '[]'::jsonb) FROM task_checkpoint c))",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap()
}

fn omitted_gap(gaps: &[Gap]) {
    let omitted: Vec<_> = gaps.iter().filter(|gap| gap.message == OMITTED).collect();
    assert_eq!(omitted.len(), 1, "{gaps:?}");
    assert_eq!(omitted[0].reason, GapReason::NotFound);
    assert!(omitted[0].project.is_none());
}

fn excludes<T: Serialize>(value: &T, forbidden: &[&str]) {
    let json = serde_json::to_string(value).unwrap();
    for text in forbidden {
        assert!(
            !json.contains(text),
            "a saved unauthorized reference was returned"
        );
    }
}

fn retained(record: &MemoryRecord, saved: &Evidence, state: IndexState) {
    let mut expected = saved.clone();
    expected.index_state = state;
    assert_eq!(record.evidence, vec![expected]);
    assert_eq!(record.related_projects, vec![name(VISIBLE)]);
}

fn stored_record_id(id: &MemoryId) -> KnowledgeRecordId {
    KnowledgeRecordId(uuid::Uuid::parse_str(id.as_str()).unwrap())
}

async fn assert_no_namesake_rationale(engine: &Engine, public: &Workspace, private: &MemoryRecord) {
    let history = engine
        .history(
            &alice_caller(),
            HistoryInput {
                target: target(public),
                project: Some(name(VISIBLE)),
                path: Some(RepoPath::new("src/private-pointer.ts").unwrap()),
                include: vec![HistoryFacet::Rationale],
                ..HistoryInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        history.rationale.is_empty(),
        "a private workspace's same-name evidence is not public file rationale"
    );
    assert!(
        history
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::NoMatches)
    );
    omitted_gap(&history.gaps);
    excludes(
        &history,
        &[
            "private-space",
            private.id.as_str(),
            private.evidence[0].commit.as_str(),
        ],
    );
}

async fn mark_stale(engine: &Engine, id: &MemoryId) {
    let mut conn = engine.store().acquire().await.unwrap();
    let record = knowledge::get_record(&mut conn, stored_record_id(id))
        .await
        .unwrap()
        .unwrap();
    let at = record.updated_at + time::Duration::seconds(1);
    knowledge::update_record(
        &mut conn,
        &RecordUpdate {
            id: record.id,
            expected_version: record.version,
            expected_revision: record.revision,
            state: KnowledgeState::Stale,
            pinned: record.pinned,
            related_symbols: record.related_symbols,
            superseded_by: record.superseded_by,
            content: None,
            history: vec![HistoryEntry {
                at,
                actor: record.author,
                action: KnowledgeAction::MarkStale,
                from: Some(record.state),
                to: KnowledgeState::Stale,
                version: record.version,
                reason: "synthetic task checkpoint regression".to_owned(),
            }],
            updated_at: at,
        },
    )
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persisted_memory_rechecks_source_grants_for_cached_and_fresh_reads() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace();
    let data = tempfile::tempdir().unwrap();
    let grants = Arc::new(CurrentAccess::new());
    let original = engine(&db, &ws, data.path(), grants.clone()).await;
    index(&original, &ws).await;
    let visible = source_id(&original, &ws, VISIBLE, "VisibleMemoryProbe").await;
    let hidden = source_id(&original, &ws, HIDDEN, "HiddenMemoryProbe").await;
    let mixed = write_record(
        &original,
        &ws,
        ScopeLevel::User,
        None,
        "shared rationale",
        vec![visible, hidden.clone()],
    )
    .await;
    assert_eq!(mixed.evidence.len(), 2);
    let saved = mixed
        .evidence
        .iter()
        .find(|evidence| evidence.project.as_str() == VISIBLE)
        .unwrap()
        .clone();
    let hidden_commit = mixed
        .evidence
        .iter()
        .find(|evidence| evidence.project.as_str() == HIDDEN)
        .unwrap()
        .commit
        .clone();
    let hidden_record = write_record(
        &original,
        &ws,
        ScopeLevel::Project,
        Some(HIDDEN),
        "hidden record body marker",
        vec![hidden],
    )
    .await;
    let opened = original
        .open_workspace(
            &alice_caller(),
            OpenWorkspaceInput {
                workspace: Some(ws.resolved.name.clone()),
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap();
    grants.restrict(ws.resolved.name.as_str(), VISIBLE);
    let before = snapshot(&original).await;
    let stats = original.indexer().stats();
    let cached = original
        .read_memory(
            &alice_caller(),
            ReadMemoryInput {
                target: Target::context(opened.context_id),
                ids: vec![mixed.id.clone()],
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(cached.records.len(), 1);
    retained(&cached.records[0], &saved, IndexState::Current);
    omitted_gap(&cached.gaps);
    excludes(
        &cached,
        &[
            HIDDEN,
            HIDDEN_PATH,
            hidden_record.id.as_str(),
            hidden_commit.as_str(),
        ],
    );
    assert_eq!(snapshot(&original).await, before);
    assert_eq!(original.indexer().stats(), stats);
    drop(original);

    let reader = engine(
        &db,
        &ws,
        data.path(),
        Arc::new(project_access(ws.resolved.name.as_str(), VISIBLE)),
    )
    .await;
    let before = snapshot(&reader).await;
    let stats = reader.indexer().stats();
    let fresh = reader
        .read_memory(
            &alice_caller(),
            ReadMemoryInput {
                target: target(&ws),
                ids: vec![mixed.id.clone()],
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(fresh, cached);
    let listed = reader
        .read_memory(
            &alice_caller(),
            ReadMemoryInput {
                target: target(&ws),
                query: Some(QUERY.to_owned()),
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(listed.records, fresh.records);
    omitted_gap(&listed.gaps);
    let found = reader
        .search(
            &alice_caller(),
            SearchInput {
                target: target(&ws),
                query: QUERY.to_owned(),
                kinds: vec![SearchKind::Memory],
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(found.hits.is_empty());
    assert_eq!(found.memory_hits.len(), 1);
    assert_eq!(found.memory_hits[0].record, fresh.records[0]);
    excludes(
        &found,
        &[
            HIDDEN,
            HIDDEN_PATH,
            "hidden record body marker",
            hidden_commit.as_str(),
        ],
    );

    let mut empty = Vec::new();
    for id in [
        hidden_record.id,
        MemoryId::new(uuid::Uuid::now_v7().to_string()).unwrap(),
    ] {
        empty.push(
            reader
                .read_memory(
                    &alice_caller(),
                    ReadMemoryInput {
                        target: target(&ws),
                        ids: vec![id],
                        ..ReadMemoryInput::default()
                    },
                )
                .await
                .unwrap(),
        );
    }
    assert_eq!(empty[0], empty[1]);
    assert!(empty[0].records.is_empty());
    excludes(&empty, &[HIDDEN, HIDDEN_PATH, "hidden record body marker"]);
    assert_eq!(snapshot(&reader).await, before);
    assert_eq!(reader.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn resume_filters_linked_decisions_checkpoint_ids_and_historical_manifests() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace();
    let data = tempfile::tempdir().unwrap();
    let original = engine(&db, &ws, data.path(), Arc::new(access())).await;
    index(&original, &ws).await;
    let visible_id = source_id(&original, &ws, VISIBLE, "VisibleMemoryProbe").await;
    let hidden_id = source_id(&original, &ws, HIDDEN, "HiddenMemoryProbe").await;
    let own = write_record(
        &original,
        &ws,
        ScopeLevel::Project,
        Some(VISIBLE),
        "visible linked decision",
        vec![visible_id.clone()],
    )
    .await;
    let hidden = write_record(
        &original,
        &ws,
        ScopeLevel::Project,
        Some(HIDDEN),
        "hidden linked decision marker",
        vec![hidden_id.clone()],
    )
    .await;
    let hidden_checkpoint = write_record(
        &original,
        &ws,
        ScopeLevel::Project,
        Some(HIDDEN),
        "hidden checkpoint-only decision marker",
        vec![hidden_id],
    )
    .await;
    let saved = original
        .save_checkpoint(
            &alice_caller(),
            SaveCheckpointInput {
                target: target(&ws),
                title: Some("Synthetic visible task".to_owned()),
                goal: Some("Preserve sourced decisions across sessions".to_owned()),
                progress: "Saved the initial synthetic task".to_owned(),
                decisions: vec![DecisionInput {
                    title: "owned task decision".to_owned(),
                    body: "This decision belongs to the resumed task.".to_owned(),
                    evidence: vec![visible_id.clone()],
                }],
                next_steps: vec!["Read the retained source pointer".to_owned()],
                idempotency_key: Some("visibility-task-receipt".to_owned()),
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(saved.decisions.len(), 1);
    assert_eq!(saved.manifest.len(), 2);
    let second = original
        .save_checkpoint(
            &alice_caller(),
            SaveCheckpointInput {
                target: target(&ws),
                task_id: Some(saved.task_id.clone()),
                progress: "Recorded a task-scope checkpoint-only decision".to_owned(),
                decisions: vec![DecisionInput {
                    title: "visible checkpoint-only decision".to_owned(),
                    body: "This saved task decision remains referenced by its checkpoint."
                        .to_owned(),
                    evidence: vec![visible_id],
                }],
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(second.decisions.len(), 1);
    let checkpoint_only = second.decisions[0].clone();
    assert_eq!(checkpoint_only.scope.level, ScopeLevel::Task);
    mark_stale(&original, &checkpoint_only.id).await;
    mark_stale(&original, &hidden_checkpoint.id).await;
    let task_id = TaskId(uuid::Uuid::parse_str(saved.task_id.as_str()).unwrap());
    let mut conn = original.store().acquire().await.unwrap();
    let stored = tasks::get_task(&mut conn, task_id).await.unwrap().unwrap();
    let mut details = stored.details.clone();
    let missing_decision = KnowledgeRecordId(uuid::Uuid::now_v7());
    // Retain this decision only in checkpoint history, so consulting just the
    // current task's decision list cannot produce its stale warning.
    details
        .decisions
        .retain(|id| *id != stored_record_id(&checkpoint_only.id));
    details.decisions.extend([
        stored_record_id(&own.id),
        stored_record_id(&hidden.id),
        missing_decision,
    ]);
    tasks::update_task(
        &mut conn,
        &TaskUpdate {
            id: stored.id,
            expected_revision: stored.revision,
            title: stored.title.clone(),
            goal: stored.goal.clone(),
            status: stored.status,
            details,
            updated_at: stored.updated_at,
        },
    )
    .await
    .unwrap();
    let checkpoint = tasks::append_checkpoint(
        &mut conn,
        &NewCheckpoint {
            task: stored.id,
            at: stored.updated_at + time::Duration::seconds(2),
            summary: "Checkpoint links are re-authorized on resume".to_owned(),
            decisions: vec![
                stored_record_id(&checkpoint_only.id),
                stored_record_id(&hidden_checkpoint.id),
            ],
            next_steps: vec!["Read only currently permitted evidence".to_owned()],
            manifest: stored.details.view_manifest,
        },
    )
    .await
    .unwrap();
    assert_eq!(checkpoint.seq, 3);
    drop(conn);
    let before_retry = snapshot(&original).await;
    let retry_stats = original.indexer().stats();
    let rejected = original
        .save_checkpoint(
            &alice_caller(),
            SaveCheckpointInput {
                target: target(&ws),
                goal: Some("Preserve sourced decisions across sessions".to_owned()),
                progress: "Saved the initial synthetic task".to_owned(),
                idempotency_key: Some("visibility-task-receipt".to_owned()),
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap_err();
    assert!(matches!(rejected, ToolError::NotFound(_)));
    assert!(!rejected.to_string().contains(&missing_decision.to_string()));
    assert_eq!(snapshot(&original).await, before_retry);
    assert_eq!(original.indexer().stats(), retry_stats);
    drop(original);

    // An unauthorized source must never be probed while reconstructing the
    // saved manifest or its changed-since digest.
    std::fs::rename(
        ws.project_dir(HIDDEN),
        ws.root().join("parked-hidden-source"),
    )
    .unwrap();
    let reader = engine(
        &db,
        &ws,
        data.path(),
        Arc::new(project_access(ws.resolved.name.as_str(), VISIBLE)),
    )
    .await;
    let before = snapshot(&reader).await;
    let stats = reader.indexer().stats();
    let resumed = reader
        .resume_task(
            &alice_caller(),
            ResumeTaskInput {
                target: target(&ws),
                task_id: Some(saved.task_id.clone()),
                ..ResumeTaskInput::default()
            },
        )
        .await
        .unwrap();
    omitted_gap(&resumed.gaps);
    let task = resumed.task.as_ref().unwrap();
    assert_eq!(task.summary.task_id, saved.task_id);
    assert_eq!(task.checkpoints.len(), 3);
    assert_eq!(task.manifest.len(), 1);
    assert_eq!(task.manifest[0].project, name(VISIBLE));
    assert_eq!(task.manifest[0].index_state, IndexState::Current);
    assert_eq!(
        task.manifest[0].commit,
        Some(own.evidence[0].commit.clone())
    );
    assert!(task.changed_since.is_empty());
    assert!(task.decisions.iter().any(|record| record.id == own.id));
    assert!(
        task.decisions
            .iter()
            .any(|record| record.id == saved.decisions[0].id)
    );
    assert!(
        task.stale_knowledge.iter().any(|record| {
            record.id == checkpoint_only.id && record.status == MemoryStatus::Stale
        })
    );
    excludes(
        &resumed,
        &[
            HIDDEN,
            HIDDEN_PATH,
            hidden.id.as_str(),
            hidden_checkpoint.id.as_str(),
            hidden.evidence[0].commit.as_str(),
            &missing_decision.to_string(),
            "hidden linked decision marker",
            "hidden checkpoint-only decision marker",
        ],
    );
    assert_eq!(snapshot(&reader).await, before);
    assert_eq!(reader.indexer().stats(), stats);

    write_source(&ws, VISIBLE, VISIBLE_PATH, "VisibleMemoryProbeAdvanced");
    let advanced = ws.commit_all(VISIBLE, "advance the visible task source without indexing");
    assert_ne!(advanced, own.evidence[0].commit.as_str());
    let stale = reader
        .resume_task(
            &alice_caller(),
            ResumeTaskInput {
                target: target(&ws),
                task_id: Some(saved.task_id),
                ..ResumeTaskInput::default()
            },
        )
        .await
        .unwrap();
    let task = stale.task.unwrap();
    assert_eq!(task.manifest[0].index_state, IndexState::Stale);
    assert_eq!(
        task.manifest[0].commit,
        Some(own.evidence[0].commit.clone())
    );
    assert!(task.decisions.iter().all(|record| {
        record
            .evidence
            .iter()
            .all(|evidence| evidence.index_state == IndexState::Stale)
    }));
    assert_eq!(snapshot(&reader).await, before);
    assert_eq!(reader.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn authorized_saved_pointers_survive_missing_refs_and_unindexed_views() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == VISIBLE);
    let data = tempfile::tempdir().unwrap();
    let original = engine(&db, &ws, data.path(), Arc::new(access())).await;
    index(&original, &ws).await;
    let source = source_id(&original, &ws, VISIBLE, "VisibleMemoryProbe").await;
    let memory = write_record(
        &original,
        &ws,
        ScopeLevel::User,
        None,
        "saved pointer",
        vec![source.clone()],
    )
    .await;
    let saved = original
        .save_checkpoint(
            &alice_caller(),
            SaveCheckpointInput {
                target: target(&ws),
                goal: Some("Keep a historical source pointer".to_owned()),
                progress: "Saved the source before a ref disappears".to_owned(),
                decisions: vec![DecisionInput {
                    title: "retained task pointer".to_owned(),
                    body: "The source pointer remains useful when the index is unavailable."
                        .to_owned(),
                    evidence: vec![source],
                }],
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap();
    let pointer = memory.evidence[0].clone();
    drop(original);

    ws.git(VISIBLE, &["update-ref", "-d", "refs/heads/main"]);
    for reason in [GapReason::RefNotFound, GapReason::ProjectNotIndexed] {
        if reason == GapReason::ProjectNotIndexed {
            ws.git(
                VISIBLE,
                &["update-ref", "refs/heads/main", pointer.commit.as_str()],
            );
            ws.git(
                VISIBLE,
                &["update-ref", "refs/heads/pending", pointer.commit.as_str()],
            );
            ws.resolved.projects[0].track.value = "branch:pending".parse().unwrap();
        }
        let reader = engine(
            &db,
            &ws,
            data.path(),
            Arc::new(project_access(ws.resolved.name.as_str(), VISIBLE)),
        )
        .await;
        let before = snapshot(&reader).await;
        let stats = reader.indexer().stats();
        let read = reader
            .read_memory(
                &alice_caller(),
                ReadMemoryInput {
                    target: target(&ws),
                    ids: vec![memory.id.clone()],
                    ..ReadMemoryInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(read.records.len(), 1);
        retained(&read.records[0], &pointer, IndexState::NotIndexed);
        assert!(
            read.gaps.iter().any(|gap| {
                gap.reason == reason && gap.project.as_ref() == Some(&name(VISIBLE))
            })
        );
        assert!(read.gaps.iter().all(|gap| gap.message != OMITTED));
        let found = reader
            .search(
                &alice_caller(),
                SearchInput {
                    target: target(&ws),
                    query: QUERY.to_owned(),
                    kinds: vec![SearchKind::Memory],
                    ..SearchInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(found.memory_hits.len(), 1);
        retained(
            &found.memory_hits[0].record,
            &pointer,
            IndexState::NotIndexed,
        );
        assert!(found.gaps.iter().any(|gap| gap.reason == reason));
        let resumed = reader
            .resume_task(
                &alice_caller(),
                ResumeTaskInput {
                    target: target(&ws),
                    task_id: Some(saved.task_id.clone()),
                    ..ResumeTaskInput::default()
                },
            )
            .await
            .unwrap();
        assert!(resumed.gaps.iter().any(|gap| gap.reason == reason));
        let detail = resumed.task.unwrap();
        assert_eq!(detail.manifest.len(), 1);
        assert_eq!(detail.manifest[0].commit, Some(pointer.commit.clone()));
        assert_eq!(detail.manifest[0].view, pointer.view);
        assert_eq!(detail.manifest[0].index_state, IndexState::NotIndexed);
        assert_eq!(detail.decisions.len(), 1);
        retained(&detail.decisions[0], &pointer, IndexState::NotIndexed);
        let list = reader
            .resume_task(
                &alice_caller(),
                ResumeTaskInput {
                    target: target(&ws),
                    ..ResumeTaskInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(list.tasks.len(), 1);
        assert!(list.gaps.iter().any(|gap| gap.reason == reason));
        assert_eq!(snapshot(&reader).await, before);
        assert_eq!(reader.indexer().stats(), stats);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn persisted_project_identity_is_not_rebound_to_an_equal_name_in_another_workspace() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut private = workspace();
    private.resolved.name = name("private-space");
    private
        .resolved
        .projects
        .retain(|project| project.name.as_str() == VISIBLE);
    write_source(
        &private,
        VISIBLE,
        "src/private-pointer.ts",
        "PrivateMemoryProbe",
    );
    private.commit_all(VISIBLE, "add synthetic private workspace pointer");
    let mut public = workspace();
    public.resolved.name = name("public-space");
    public
        .resolved
        .projects
        .retain(|project| project.name.as_str() == VISIBLE);
    write_source(
        &public,
        VISIBLE,
        "src/public-pointer.ts",
        "PublicMemoryProbe",
    );
    // Identical names and paths in different workspaces do not identify the
    // same source. The public file deliberately contains a different symbol.
    write_source(
        &public,
        VISIBLE,
        "src/private-pointer.ts",
        "PublicNamesakeMemoryProbe",
    );
    public.commit_all(VISIBLE, "add synthetic public workspace pointer");
    let data = tempfile::tempdir().unwrap();
    let original = engine(&db, &private, data.path(), Arc::new(access())).await;
    index(&original, &private).await;
    let private_id = source_id(&original, &private, VISIBLE, "PrivateMemoryProbe").await;
    let saved = write_record(
        &original,
        &private,
        ScopeLevel::User,
        None,
        "neutral rationale",
        vec![private_id.clone()],
    )
    .await;
    assert_eq!(saved.evidence.len(), 1);
    let pointer = saved.evidence[0].clone();
    let hidden_record = write_record(
        &original,
        &private,
        ScopeLevel::Project,
        Some(VISIBLE),
        "private scoped record marker",
        Vec::new(),
    )
    .await;
    original.add_workspace(&public.resolved).await.unwrap();
    index(&original, &public).await;
    assert_ne!(
        pointer.commit.as_str(),
        public.git(VISIBLE, &["rev-parse", "HEAD"])
    );
    let public_id = source_id(&original, &public, VISIBLE, "PublicMemoryProbe").await;
    for (ws, source) in [(&private, &private_id), (&public, &public_id)] {
        for (scope, label) in [
            (ScopeLevel::User, "user"),
            (ScopeLevel::Organization, "organization"),
        ] {
            // These use a separate query term so the original isolated
            // visibility and history assertions remain unchanged.
            let written = original
                .write_memory(
                    &alice_caller(),
                    WriteMemoryInput {
                        target: target(ws),
                        scope: MemoryScope {
                            level: scope,
                            project: None,
                            task_id: None,
                        },
                        kind: MemoryKind::Decision,
                        title: format!("namespace identity {label} positive"),
                        body: "A synthetic source pointer retains its actual workspace.".to_owned(),
                        related_symbols: Vec::new(),
                        evidence: vec![source.clone()],
                        supersedes: None,
                        idempotency_key: None,
                    },
                )
                .await
                .unwrap()
                .record;
            assert_eq!(written.evidence.len(), 1);
            assert_eq!(written.evidence[0].project, name(VISIBLE));
            assert_eq!(
                written.evidence[0].commit.as_str(),
                ws.git(VISIBLE, &["rev-parse", "HEAD"])
            );
            let read = original
                .read_memory(
                    &alice_caller(),
                    ReadMemoryInput {
                        target: target(ws),
                        ids: vec![written.id.clone()],
                        ..ReadMemoryInput::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(read.records, vec![written.clone()]);
            assert!(read.gaps.iter().all(|gap| gap.message != OMITTED));
            let mut conn = original.store().acquire().await.unwrap();
            let stored = knowledge::get_record(&mut conn, stored_record_id(&written.id))
                .await
                .unwrap()
                .unwrap();
            assert_eq!(stored.evidence.len(), 1);
            let project =
                knowell_store::hierarchy::get_project(&mut conn, stored.evidence[0].project)
                    .await
                    .unwrap()
                    .unwrap();
            let workspace = knowell_store::hierarchy::get_workspace(&mut conn, project.workspace)
                .await
                .unwrap()
                .unwrap();
            assert_eq!(workspace.name, ws.resolved.name);
            assert_eq!(project.name, name(VISIBLE));
            assert_eq!(
                stored.evidence[0].commit,
                written.evidence[0].commit.as_str()
            );
            assert_eq!(
                stored.evidence[0].content_hash,
                written.evidence[0].content_hash
            );
        }
    }
    let before_review = {
        let mut conn = original.store().acquire().await.unwrap();
        knowledge::get_record(&mut conn, stored_record_id(&saved.id))
            .await
            .unwrap()
            .unwrap()
    };
    let human = EngineContext {
        principal: Principal::User(alice()),
        scopes: None,
        grants: Arc::new({
            let mut grants = GrantSet::new();
            grants.add(
                Grant::new(
                    Principal::User(alice()),
                    Role::Admin,
                    ResourceScope::Organization,
                )
                .unwrap(),
            );
            grants
        }),
        visible: knowell_auth::ProjectFilter::default(),
        request_id: knowell_auth::RequestId::new("namespace-review").unwrap(),
        audit: Arc::new(knowell_server::MemoryAuditSink::new()),
    };
    let accepted = knowell_server::Engine::call(
        &original,
        &human,
        EngineRequest::DecideMemory(MemoryDecision {
            id: saved.id.to_string(),
            action: MemoryAction::Accept,
            note: Some("Accept the synthetic namespace preservation decision".to_owned()),
        }),
    )
    .await
    .unwrap();
    assert_eq!(accepted["state"], "accepted");
    let after_review = {
        let mut conn = original.store().acquire().await.unwrap();
        knowledge::get_record(&mut conn, stored_record_id(&saved.id))
            .await
            .unwrap()
            .unwrap()
    };
    assert_eq!(after_review.state, KnowledgeState::Accepted);
    assert_eq!(after_review.evidence, before_review.evidence);
    assert_eq!(after_review.scope, before_review.scope);
    assert_eq!(after_review.version, before_review.version);
    assert_eq!(after_review.revision, before_review.revision + 1);
    assert_eq!(after_review.body, before_review.body);
    assert_eq!(after_review.evidence[0].commit, pointer.commit.as_str());
    assert_eq!(after_review.evidence[0].content_hash, pointer.content_hash);
    drop(original);

    let reader = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .workspace(private.resolved.clone())
        .workspace(public.resolved.clone())
        .access(Arc::new(project_access("public-space", VISIBLE)))
        .build()
        .await
        .unwrap();
    let before = snapshot(&reader).await;
    let stats = reader.indexer().stats();
    let read = reader
        .read_memory(
            &alice_caller(),
            ReadMemoryInput {
                target: target(&public),
                ids: vec![saved.id.clone()],
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(read.records.len(), 1);
    assert_eq!(read.records[0].id, saved.id);
    assert_eq!(read.records[0].body, saved.body);
    assert!(read.records[0].evidence.is_empty());
    assert!(read.records[0].related_projects.is_empty());
    omitted_gap(&read.gaps);
    excludes(
        &read,
        &[
            "private-space",
            "src/private-pointer.ts",
            pointer.commit.as_str(),
        ],
    );
    let found = reader
        .search(
            &alice_caller(),
            SearchInput {
                target: target(&public),
                query: QUERY.to_owned(),
                kinds: vec![SearchKind::Memory],
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(found.memory_hits.len(), 1);
    assert_eq!(found.memory_hits[0].record, read.records[0]);
    excludes(
        &found,
        &[
            "private-space",
            "src/private-pointer.ts",
            pointer.commit.as_str(),
        ],
    );
    assert_no_namesake_rationale(&reader, &public, &saved).await;
    assert_eq!(snapshot(&reader).await, before);
    assert_eq!(reader.indexer().stats(), stats);
    drop(reader);

    // This engine does not register the evidence's workspace at all. Loading
    // exact database identity must not turn that into a public namesake or
    // discard the readable outer user record.
    std::fs::rename(
        private.project_dir(VISIBLE),
        private.root().join("parked-private-source"),
    )
    .unwrap();
    let reader = engine(
        &db,
        &public,
        data.path(),
        Arc::new(project_access("public-space", VISIBLE)),
    )
    .await;
    let before = snapshot(&reader).await;
    let stats = reader.indexer().stats();
    let unregistered = reader
        .read_memory(
            &alice_caller(),
            ReadMemoryInput {
                target: target(&public),
                ids: vec![saved.id.clone()],
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(unregistered, read);
    omitted_gap(&unregistered.gaps);
    let found = reader
        .search(
            &alice_caller(),
            SearchInput {
                target: target(&public),
                query: QUERY.to_owned(),
                kinds: vec![SearchKind::Memory],
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(found.memory_hits.len(), 1);
    assert_eq!(found.memory_hits[0].record, unregistered.records[0]);
    omitted_gap(&found.gaps);
    assert_no_namesake_rationale(&reader, &public, &saved).await;
    let mut inaccessible = Vec::new();
    for id in [
        hidden_record.id,
        MemoryId::new(uuid::Uuid::now_v7().to_string()).unwrap(),
    ] {
        inaccessible.push(
            reader
                .read_memory(
                    &alice_caller(),
                    ReadMemoryInput {
                        target: target(&public),
                        ids: vec![id],
                        ..ReadMemoryInput::default()
                    },
                )
                .await
                .unwrap(),
        );
    }
    assert_eq!(inaccessible[0], inaccessible[1]);
    assert!(inaccessible[0].records.is_empty());
    excludes(
        &inaccessible,
        &["private-space", "private scoped record marker"],
    );
    assert_eq!(snapshot(&reader).await, before);
    assert_eq!(reader.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn shared_in_memory_records_preserve_origin_and_never_rebind_unknown_evidence() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut private = workspace();
    private.resolved.name = name("private-space");
    private
        .resolved
        .projects
        .retain(|project| project.name.as_str() == VISIBLE);
    let mut public = workspace();
    public.resolved.name = name("public-space");
    public
        .resolved
        .projects
        .retain(|project| project.name.as_str() == VISIBLE);
    let path = "src/namesake-memory.ts";
    write_source(&private, VISIBLE, path, "PrivateNamesakeMemoryProbe");
    private.commit_all(VISIBLE, "add synthetic in-memory origin pointer");
    write_source(&public, VISIBLE, path, "PublicNamesakeMemoryProbe");
    public.commit_all(VISIBLE, "add synthetic in-memory namesake pointer");
    let data = tempfile::tempdir().unwrap();
    let memory = Arc::new(InMemoryMemory::new());
    let original = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .workspace(private.resolved.clone())
        .access(Arc::new(access()))
        .memory(memory.clone())
        .build()
        .await
        .unwrap();
    index(&original, &private).await;
    let private_id = source_id(&original, &private, VISIBLE, "PrivateNamesakeMemoryProbe").await;
    let mut saved = Vec::new();
    let mut legacy = Vec::new();
    for (scope, label) in [
        (ScopeLevel::User, "user origin"),
        (ScopeLevel::Organization, "organization origin"),
    ] {
        let record = write_record(
            &original,
            &private,
            scope,
            None,
            label,
            vec![private_id.clone()],
        )
        .await;
        assert_eq!(record.evidence.len(), 1);
        let id = knowell_knowledge::RecordId::from_uuid(
            uuid::Uuid::parse_str(record.id.as_str()).unwrap(),
        );
        let mut row = memory.get_record(id).await.unwrap().unwrap();
        assert_eq!(
            row.evidence_workspaces,
            vec![Some(private.resolved.name.clone())]
        );
        if scope == ScopeLevel::User {
            // An ordinary metadata update must preserve captured provenance.
            let mut changed = row.record.clone();
            changed.pinned = true;
            let updated = memory.update_record(&row, &changed).await.unwrap();
            assert_eq!(updated.evidence_workspaces, row.evidence_workspaces);
            assert_eq!(updated.record.evidence, row.record.evidence);
            row = updated;
        }
        let read = original
            .read_memory(
                &alice_caller(),
                ReadMemoryInput {
                    target: target(&private),
                    ids: vec![record.id.clone()],
                    ..ReadMemoryInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(read.records.len(), 1);
        retained(&read.records[0], &record.evidence[0], IndexState::Current);
        assert_eq!(read.records[0].body, record.body);
        assert!(read.gaps.iter().all(|gap| gap.message != OMITTED));
        // Legacy insertion has no workspace context. Its namespace must stay
        // unknown even when a future reader registers only one namesake.
        let mut unknown = row.record.clone();
        unknown.id = knowell_knowledge::RecordId::generate();
        unknown.title = format!("{QUERY} legacy {label}");
        unknown.body = format!("{QUERY} legacy {label} retains its untrusted body.");
        let unknown = memory.insert_record(&unknown).await.unwrap();
        assert_eq!(unknown.evidence_workspaces, vec![None]);
        legacy.push(unknown);
        saved.push(record);
    }
    let private_commit = saved[0].evidence[0].commit.clone();
    drop(original);
    std::fs::rename(
        private.project_dir(VISIBLE),
        private.root().join("parked-private-in-memory-source"),
    )
    .unwrap();

    // This fresh engine knows only the public workspace. Sharing the memory
    // repository must not make its unique project name a namespace authority.
    let writer = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .workspace(public.resolved.clone())
        .access(Arc::new(access()))
        .memory(memory.clone())
        .build()
        .await
        .unwrap();
    index(&writer, &public).await;
    let public_id = source_id(&writer, &public, VISIBLE, "PublicNamesakeMemoryProbe").await;
    let mut positive = Vec::new();
    for (scope, label) in [
        (ScopeLevel::User, "user namesake"),
        (ScopeLevel::Organization, "organization namesake"),
    ] {
        let record = write_record(
            &writer,
            &public,
            scope,
            None,
            label,
            vec![public_id.clone()],
        )
        .await;
        assert_eq!(record.evidence.len(), 1);
        assert_ne!(record.evidence[0].commit, private_commit);
        positive.push(record);
    }
    drop(writer);
    let mut grants = GrantSet::new();
    grants.add(
        Grant::new(
            Principal::User(alice()),
            Role::Member,
            ResourceScope::workspace(public.resolved.name.clone()),
        )
        .unwrap(),
    );
    let reader = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .workspace(public.resolved.clone())
        .access(Arc::new(StaticAccess::local_user(alice(), grants)))
        .memory(memory.clone())
        .build()
        .await
        .unwrap();
    let before = snapshot(&reader).await;
    let stats = reader.indexer().stats();
    let mut rows = Vec::new();
    for record in saved.iter().chain(&positive) {
        let id = knowell_knowledge::RecordId::from_uuid(
            uuid::Uuid::parse_str(record.id.as_str()).unwrap(),
        );
        let row = memory.get_record(id).await.unwrap().unwrap();
        let expected = if saved.iter().any(|old| old.id == record.id) {
            &private.resolved.name
        } else {
            &public.resolved.name
        };
        assert_eq!(row.evidence_workspaces, vec![Some(expected.clone())]);
        rows.push(row);
    }
    rows.extend(legacy);
    let mut returned = Vec::new();
    for row in &rows {
        let id = MemoryId::new(row.record.id.to_string()).unwrap();
        let read = reader
            .read_memory(
                &alice_caller(),
                ReadMemoryInput {
                    target: target(&public),
                    ids: vec![id.clone()],
                    ..ReadMemoryInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(read.records.len(), 1);
        let record = &read.records[0];
        assert_eq!(record.id, id);
        assert_eq!(record.body.text(), row.record.body);
        if let Some(saved) = positive.iter().find(|saved| saved.id == id) {
            retained(record, &saved.evidence[0], IndexState::Current);
            assert!(read.gaps.iter().all(|gap| gap.message != OMITTED));
        } else {
            assert!(record.evidence.is_empty());
            assert!(record.related_projects.is_empty());
            omitted_gap(&read.gaps);
        }
        excludes(&read, &["private-space", private_commit.as_str()]);
        returned.push(record.clone());
    }
    let found = reader
        .search(
            &alice_caller(),
            SearchInput {
                target: target(&public),
                query: QUERY.to_owned(),
                kinds: vec![SearchKind::Memory],
                limit: Some(100),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(found.memory_hits.len(), returned.len());
    for hit in &found.memory_hits {
        assert_eq!(
            returned.iter().find(|record| record.id == hit.record.id),
            Some(&hit.record)
        );
    }
    omitted_gap(&found.gaps);
    excludes(&found, &["private-space", private_commit.as_str()]);
    let history = reader
        .history(
            &alice_caller(),
            HistoryInput {
                target: target(&public),
                project: Some(name(VISIBLE)),
                path: Some(RepoPath::new(path).unwrap()),
                include: vec![HistoryFacet::Rationale],
                ..HistoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(history.rationale.len(), positive.len());
    for record in &history.rationale {
        let saved = positive.iter().find(|saved| saved.id == record.id).unwrap();
        retained(record, &saved.evidence[0], IndexState::Current);
    }
    omitted_gap(&history.gaps);
    excludes(&history, &["private-space", private_commit.as_str()]);
    for row in rows {
        assert_eq!(memory.get_record(row.record.id).await.unwrap(), Some(row));
    }
    assert_eq!(snapshot(&reader).await, before);
    assert_eq!(reader.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn personal_manifest_history_is_private_to_its_task_owner() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == VISIBLE);
    let data = tempfile::tempdir().unwrap();
    let original = engine(&db, &ws, data.path(), Arc::new(access())).await;
    index(&original, &ws).await;
    let saved = original
        .save_checkpoint(
            &alice_caller(),
            SaveCheckpointInput {
                target: target(&ws),
                goal: Some("Synthetic personal source history".to_owned()),
                progress: "A personal manifest is not a shared source grant".to_owned(),
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap();
    let mut conn = original.store().acquire().await.unwrap();
    let source = tasks::get_task(
        &mut conn,
        TaskId(uuid::Uuid::parse_str(saved.task_id.as_str()).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let mut details = source.details.clone();
    let manifest = details.view_manifest.as_array_mut().unwrap();
    assert_eq!(manifest.len(), 1);
    manifest[0]["local_generation"] = serde_json::Value::from(77);
    let mut task_ids = Vec::new();
    for owner in [None, Some(alice().to_string())] {
        let at = source.updated_at + time::Duration::seconds(1);
        let row = tasks::create_task(
            &mut conn,
            &NewTask {
                id: TaskId(uuid::Uuid::now_v7()),
                organization: source.organization,
                workspace: source.workspace,
                owner,
                title: "Synthetic saved personal task".to_owned(),
                goal: "Retain only authorized personal manifest history".to_owned(),
                status: StoredTaskStatus::InProgress,
                details: details.clone(),
                created_at: at,
                updated_at: at,
            },
        )
        .await
        .unwrap();
        tasks::append_checkpoint(
            &mut conn,
            &NewCheckpoint {
                task: row.id,
                at,
                summary: "Synthetic personal manifest checkpoint".to_owned(),
                decisions: Vec::new(),
                next_steps: Vec::new(),
                manifest: details.view_manifest.clone(),
            },
        )
        .await
        .unwrap();
        task_ids.push(row.id);
    }
    drop(conn);
    drop(original);
    let reader = engine(
        &db,
        &ws,
        data.path(),
        Arc::new(project_access(ws.resolved.name.as_str(), VISIBLE)),
    )
    .await;
    let before = snapshot(&reader).await;
    let stats = reader.indexer().stats();
    for (index, id) in task_ids.iter().enumerate() {
        let output = reader
            .resume_task(
                &alice_caller(),
                ResumeTaskInput {
                    target: target(&ws),
                    task_id: Some(knowell_mcp::TaskId::new(id.to_string()).unwrap()),
                    ..ResumeTaskInput::default()
                },
            )
            .await
            .unwrap();
        let detail = output.task.as_ref().unwrap();
        assert_eq!(detail.checkpoints.len(), 1);
        if index == 0 {
            assert!(detail.manifest.is_empty());
            omitted_gap(&output.gaps);
            let json = serde_json::to_value(&output).unwrap();
            assert!(
                json["task"]
                    .get("manifest")
                    .is_none_or(|value| value.as_array().unwrap().is_empty())
            );
        } else {
            assert_eq!(detail.manifest.len(), 1);
            assert_eq!(detail.manifest[0].project, name(VISIBLE));
            assert_eq!(detail.manifest[0].layer, ViewLayer::Personal);
            assert_eq!(detail.manifest[0].local_generation, 77);
            assert_eq!(detail.manifest[0].index_state, IndexState::Stale);
            assert_eq!(detail.manifest[0].commit, saved.manifest[0].commit);
            assert!(output.gaps.iter().all(|gap| gap.message != OMITTED));
        }
    }
    assert_eq!(snapshot(&reader).await, before);
    assert_eq!(reader.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn malformed_saved_reference_labels_are_omitted_with_an_explicit_gap() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == VISIBLE);
    let data = tempfile::tempdir().unwrap();
    let original = engine(&db, &ws, data.path(), Arc::new(access())).await;
    index(&original, &ws).await;
    let source = source_id(&original, &ws, VISIBLE, "VisibleMemoryProbe").await;
    let saved = original
        .save_checkpoint(
            &alice_caller(),
            SaveCheckpointInput {
                target: target(&ws),
                goal: Some("Preserve honest saved reference gaps".to_owned()),
                progress: "Saved a synthetic authorized source pointer".to_owned(),
                decisions: vec![DecisionInput {
                    title: "saved reference conversion decision".to_owned(),
                    body: "The untrusted decision text survives incomplete source metadata."
                        .to_owned(),
                    evidence: vec![source],
                }],
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(saved.decisions.len(), 1);
    let decision = &saved.decisions[0];
    assert_eq!(decision.evidence.len(), 1);
    let pointer = decision.evidence[0].clone();
    let manifest_only_id = TaskId(uuid::Uuid::now_v7());
    // A separate bounded marker permits checking diagnostics without also
    // rejecting the prefix of the valid full commit that must be retained.
    let legitimate = format!(
        "{} {} {}",
        serde_json::to_string(&saved).unwrap(),
        manifest_only_id,
        manifest_only_id.0.simple(),
    );
    let short_commit = (0xabc0000_u32..0xabc1000)
        .map(|value| format!("{value:07x}"))
        .find(|value| !legitimate.contains(value))
        .unwrap();
    let mut conn = original.store().acquire().await.unwrap();
    let record = knowledge::get_record(&mut conn, stored_record_id(&decision.id))
        .await
        .unwrap()
        .unwrap();
    let mut evidence = record.evidence.clone();
    let mut invalid = evidence[0].clone();
    invalid.view = "synthetic-invalid-ref".to_owned();
    evidence.push(invalid);
    let mut short = evidence[0].clone();
    short.commit = short_commit.clone();
    evidence.push(short);
    let at = record.updated_at + time::Duration::seconds(1);
    knowledge::update_record(
        &mut conn,
        &RecordUpdate {
            id: record.id,
            expected_version: record.version,
            expected_revision: record.revision,
            state: record.state,
            pinned: record.pinned,
            related_symbols: record.related_symbols.clone(),
            superseded_by: record.superseded_by,
            content: Some(RecordContent {
                title: record.title,
                body: record.body,
                tags: record.tags,
                evidence,
            }),
            history: vec![HistoryEntry {
                at,
                actor: record.author,
                action: KnowledgeAction::Edit,
                from: Some(record.state),
                to: record.state,
                version: record.version + 1,
                reason: "synthetic bounded stored reference conversion fixture".to_owned(),
            }],
            updated_at: at,
        },
    )
    .await
    .unwrap();
    let task = tasks::get_task(
        &mut conn,
        TaskId(uuid::Uuid::parse_str(saved.task_id.as_str()).unwrap()),
    )
    .await
    .unwrap()
    .unwrap();
    let mut details = task.details.clone();
    let manifest = details.view_manifest.as_array_mut().unwrap();
    assert_eq!(manifest.len(), 1);
    let mut invalid = manifest[0].clone();
    invalid["view"] = serde_json::Value::from("synthetic-invalid-ref");
    manifest.push(invalid);
    let mut short = manifest[0].clone();
    short["commit"] = serde_json::Value::from(short_commit.clone());
    manifest.push(short);
    tasks::update_task(
        &mut conn,
        &TaskUpdate {
            id: task.id,
            expected_revision: task.revision,
            title: task.title.clone(),
            goal: task.goal.clone(),
            status: task.status,
            details: details.clone(),
            updated_at: task.updated_at,
        },
    )
    .await
    .unwrap();
    tasks::append_checkpoint(
        &mut conn,
        &NewCheckpoint {
            task: task.id,
            at: task.updated_at + time::Duration::seconds(2),
            summary: "Saved bounded labels require typed output validation".to_owned(),
            decisions: vec![stored_record_id(&decision.id)],
            next_steps: Vec::new(),
            manifest: details.view_manifest.clone(),
        },
    )
    .await
    .unwrap();
    // No decision conversion can produce this task's omission gap. Its
    // malformed manifest pins must be handled independently before the digest.
    let mut manifest_only_details = details.clone();
    manifest_only_details.decisions.clear();
    let manifest_only = tasks::create_task(
        &mut conn,
        &NewTask {
            id: manifest_only_id,
            organization: task.organization,
            workspace: task.workspace,
            owner: task.owner.clone(),
            title: "Synthetic manifest-only task".to_owned(),
            goal: "The untrusted task goal survives incomplete manifest metadata.".to_owned(),
            status: task.status,
            details: manifest_only_details,
            created_at: task.created_at,
            updated_at: task.updated_at,
        },
    )
    .await
    .unwrap();
    tasks::append_checkpoint(
        &mut conn,
        &NewCheckpoint {
            task: manifest_only.id,
            at: task.updated_at + time::Duration::seconds(2),
            summary: "Manifest conversion has its own explicit omission gap".to_owned(),
            decisions: Vec::new(),
            next_steps: Vec::new(),
            manifest: details.view_manifest,
        },
    )
    .await
    .unwrap();
    drop(conn);
    mark_stale(&original, &decision.id).await;
    drop(original);

    let reader = engine(
        &db,
        &ws,
        data.path(),
        Arc::new(project_access(ws.resolved.name.as_str(), VISIBLE)),
    )
    .await;
    let before = snapshot(&reader).await;
    let stats = reader.indexer().stats();
    for state in [IndexState::Current, IndexState::NotIndexed] {
        if state == IndexState::NotIndexed {
            ws.git(VISIBLE, &["update-ref", "-d", "refs/heads/main"]);
        }
        let read = reader
            .read_memory(
                &alice_caller(),
                ReadMemoryInput {
                    target: target(&ws),
                    ids: vec![decision.id.clone()],
                    task_id: Some(saved.task_id.clone()),
                    statuses: vec![MemoryStatus::Stale],
                    ..ReadMemoryInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(read.records.len(), 1);
        assert_eq!(read.records[0].id, decision.id);
        assert_eq!(read.records[0].body, decision.body);
        assert_eq!(read.records[0].version, decision.version + 1);
        retained(&read.records[0], &pointer, state);
        omitted_gap(&read.gaps);
        excludes(&read, &["synthetic-invalid-ref", &short_commit]);
        let resumed = reader
            .resume_task(
                &alice_caller(),
                ResumeTaskInput {
                    target: target(&ws),
                    task_id: Some(saved.task_id.clone()),
                    ..ResumeTaskInput::default()
                },
            )
            .await
            .unwrap();
        omitted_gap(&resumed.gaps);
        excludes(&resumed, &["synthetic-invalid-ref", &short_commit]);
        let detail = resumed.task.unwrap();
        assert_eq!(detail.decisions, read.records);
        assert_eq!(detail.stale_knowledge, read.records);
        assert_eq!(detail.manifest.len(), 1);
        assert_eq!(detail.manifest[0].view, pointer.view);
        assert_eq!(detail.manifest[0].commit, Some(pointer.commit.clone()));
        assert_eq!(detail.manifest[0].index_state, state);
        let manifest_output = reader
            .resume_task(
                &alice_caller(),
                ResumeTaskInput {
                    target: target(&ws),
                    task_id: Some(knowell_mcp::TaskId::new(manifest_only.id.to_string()).unwrap()),
                    ..ResumeTaskInput::default()
                },
            )
            .await
            .unwrap();
        omitted_gap(&manifest_output.gaps);
        excludes(&manifest_output, &["synthetic-invalid-ref", &short_commit]);
        let manifest_detail = manifest_output.task.as_ref().unwrap();
        assert_eq!(
            manifest_detail.summary.task_id.as_str(),
            manifest_only.id.to_string()
        );
        assert_eq!(manifest_detail.summary.goal.text(), manifest_only.goal);
        assert!(manifest_detail.decisions.is_empty());
        assert!(manifest_detail.stale_knowledge.is_empty());
        assert_eq!(manifest_detail.manifest, detail.manifest);
        if state == IndexState::NotIndexed {
            for gaps in [&read.gaps, &resumed.gaps, &manifest_output.gaps] {
                assert!(gaps.iter().any(|gap| {
                    gap.reason == GapReason::RefNotFound && gap.project == Some(name(VISIBLE))
                }));
            }
        }
        assert_eq!(snapshot(&reader).await, before);
        assert_eq!(reader.indexer().stats(), stats);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn task_listing_filters_owners_before_limiting_and_preserves_subsecond_pagination() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == VISIBLE);
    let data = tempfile::tempdir().unwrap();
    let reader = engine(
        &db,
        &ws,
        data.path(),
        Arc::new(project_access(ws.resolved.name.as_str(), VISIBLE)),
    )
    .await;
    // Registration creates metadata only; this test needs no index work.
    let registration = reader.add_workspace(&ws.resolved).await.unwrap();
    assert!(registration.issues.is_empty());
    let base = OffsetDateTime::from_unix_timestamp(1_770_000_000).unwrap();
    let mut conn = reader.store().acquire().await.unwrap();
    let mut eligible = Vec::new();
    for (title, owner, offset) in [
        ("own newest task", Some(alice().to_string()), 300),
        ("shared older task", None, 200),
        ("own oldest task", Some(alice().to_string()), 100),
    ] {
        let at = base + time::Duration::microseconds(offset);
        eligible.push(
            tasks::create_task(
                &mut conn,
                &NewTask {
                    id: TaskId(uuid::Uuid::now_v7()),
                    organization: registration.organization,
                    workspace: Some(registration.workspace),
                    owner,
                    title: title.to_owned(),
                    goal: "Synthetic ownership pagination regression".to_owned(),
                    status: StoredTaskStatus::InProgress,
                    details: TaskDetails::default(),
                    created_at: at,
                    updated_at: at,
                },
            )
            .await
            .unwrap(),
        );
    }
    let foreign_ids: Vec<_> = (0..1005).map(|_| uuid::Uuid::now_v7()).collect();
    let inserted = sqlx::query(
        "INSERT INTO task (id, organization_id, workspace_id, owner, title, goal,
                           status, created_at, updated_at)
         SELECT u.id, $2, $3, $4, 'foreign-owned synthetic task ' || u.ordinal,
                'Synthetic foreign-owner crowding fixture', 'in_progress'::task_status,
                $5 + (500 + u.ordinal) * interval '1 microsecond',
                $5 + (500 + u.ordinal) * interval '1 microsecond'
         FROM unnest($1::uuid[]) WITH ORDINALITY AS u(id, ordinal)",
    )
    .bind(&foreign_ids)
    .bind(registration.organization)
    .bind(registration.workspace)
    .bind(crate::common::bob().to_string())
    .bind(base)
    .execute(&mut *conn)
    .await
    .unwrap();
    assert_eq!(inserted.rows_affected(), 1005);
    drop(conn);
    let before = snapshot(&reader).await;
    let stats = reader.indexer().stats();
    for limit in [1, 2, 3] {
        let listed = reader
            .resume_task(
                &alice_caller(),
                ResumeTaskInput {
                    target: target(&ws),
                    limit: Some(limit),
                    ..ResumeTaskInput::default()
                },
            )
            .await
            .unwrap();
        assert!(listed.task.is_none());
        assert_eq!(listed.tasks.len(), usize::try_from(limit).unwrap());
        let expected: Vec<_> = eligible
            .iter()
            .take(usize::try_from(limit).unwrap())
            .map(|row| row.id.to_string())
            .collect();
        let actual: Vec<_> = listed
            .tasks
            .iter()
            .map(|row| row.task_id.as_str().to_owned())
            .collect();
        assert_eq!(actual, expected);
        assert_eq!(
            listed
                .gaps
                .iter()
                .any(|gap| gap.reason == GapReason::LimitReached),
            limit < 3,
        );
        assert!(listed.gaps.iter().any(|gap| {
            gap.reason == GapReason::ProjectNotIndexed && gap.project == Some(name(VISIBLE))
        }));
        assert!(
            listed
                .gaps
                .iter()
                .all(|gap| gap.reason != GapReason::NoMatches)
        );
        excludes(&listed, &["foreign-owned", "foreign-owner crowding"]);
    }
    assert_eq!(snapshot(&reader).await, before);
    assert_eq!(reader.indexer().stats(), stats);
}
