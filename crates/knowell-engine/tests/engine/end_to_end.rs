//! The whole engine on the acme-goods fixture: index, then every tool.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use knowell_auth::GrantSet;
use knowell_engine::{Access, HybridRetriever};
use knowell_eval::{Bm25Retriever, GrepRetriever, QuerySet, Retriever, run, walk_fixture};
use knowell_index::Priority;
use knowell_mcp::tools::{
    AnalyzeImpactInput, BuildContextInput, ChangeSubject, ContextRole, ContextSection,
    ContextSelectionStrategy, ContractsInput, EntryKind, FetchInput, HistoryInput,
    IndexStatusInput, InspectSymbolInput, MemoryKind, MemoryScope, MemoryStatus,
    OpenWorkspaceInput, ReadMemoryInput, ResumeTaskInput, SaveCheckpointInput, ScopeLevel,
    SearchInput, SearchKind, TierState, TraceFlowInput, WriteMemoryInput,
};
use knowell_mcp::{
    FileLocator, FreshnessTier, GapReason, IndexState, KnowellTools, MatchReason, SymbolRef,
    Target, ViewLayer,
};
use knowell_server::{EngineContext, EngineRequest, MemoryAction, MemoryDecision};

use crate::common::{
    alice, alice_caller, bob_caller, fixture_workspace, git_available, indexed_engine, name,
    require_db,
};

fn context_target(id: &knowell_mcp::ContextId) -> Target {
    Target::context(id.clone())
}

fn project_search(project: &str, query: &str, target: Target) -> SearchInput {
    SearchInput {
        target,
        query: query.to_owned(),
        projects: vec![name(project)],
        ..SearchInput::default()
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_source_bodies_keep_occurrence_languages_in_catalogue_and_search() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "handbook");
    let body = "// quokka_locus_marker shared note\n".repeat(16);
    for path in ["quokka-note.md", "quokka-note.rs"] {
        std::fs::write(ws.project_dir("handbook").join(path), &body).unwrap();
    }
    ws.commit_all("handbook", "add synthetic shared-body language cases");
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let project = opened
        .projects
        .iter()
        .find(|project| project.name.as_str() == "handbook")
        .unwrap();
    assert!(project.languages.iter().any(|language| language == "rust"));
    assert!(
        project
            .languages
            .iter()
            .any(|language| language == "markdown")
    );

    let mut hashes = Vec::new();
    for (language, path) in [("rust", "quokka-note.rs"), ("markdown", "quokka-note.md")] {
        let mut input = project_search(
            "handbook",
            "quokka_locus_marker",
            context_target(&opened.context_id),
        );
        input.languages = vec![language.to_owned()];
        // Other handbook documents are valid semantic neighbors. Restrict the
        // two shared-body occurrences before asserting exact path admission.
        input.path_prefixes = vec!["quokka-note.".to_owned()];
        input.include_diagnostics = Some(true);
        let result = engine.search(&caller, input).await.unwrap();
        assert!(!result.hits.is_empty(), "missing {language} occurrence");
        assert!(
            result
                .hits
                .iter()
                .all(|hit| hit.evidence.path.as_str() == path)
        );
        assert!(result.diagnostics.as_ref().unwrap().semantic_neighbors > 0);
        hashes.push(result.hits.first().unwrap().evidence.content_hash);
    }
    assert_eq!(hashes.first(), hashes.get(1));

    // This control checks occurrence admission, independently of the Source
    // selector's optional omission of repetitive displayed bodies.
    let mut admitted = project_search(
        "handbook",
        "quokka_locus_marker",
        context_target(&opened.context_id),
    );
    admitted.include_snippets = Some(false);
    let result = engine.search(&caller, admitted).await.unwrap();
    for path in ["quokka-note.md", "quokka-note.rs"] {
        assert!(
            result
                .hits
                .iter()
                .any(|hit| hit.evidence.path.as_str() == path)
        );
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn overlay_only_code_language_reports_missing_relations_on_a_document_base() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "handbook");
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let worktree = ws.dir.path().join("handbook-overlay");
    ws.git(
        "handbook",
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/overlay-coverage",
            worktree.to_str().unwrap(),
        ],
    );
    std::fs::write(
        worktree.join("overlay-guide.md"),
        "# Overlay guide\n\nSynthetic documentation.\n",
    )
    .unwrap();
    ws.git_in(&worktree, &["add", "--", "overlay-guide.md"]);
    let documents = engine
        .open_workspace(
            &caller,
            OpenWorkspaceInput {
                working_directory: Some(worktree.to_string_lossy().into_owned()),
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap();
    let document_search = engine
        .search(
            &caller,
            project_search(
                "handbook",
                "overlay-guide.md",
                context_target(&documents.context_id),
            ),
        )
        .await
        .unwrap();
    assert!(
        document_search
            .hits
            .iter()
            .any(|hit| hit.evidence.layer == ViewLayer::Personal)
    );
    assert!(
        document_search
            .gaps
            .iter()
            .all(|gap| gap.reason != GapReason::NoReferenceResolutionForLanguage)
    );
    let document_context = engine
        .build_context(
            &caller,
            BuildContextInput {
                target: context_target(&documents.context_id),
                task: Some("overlay-guide.md".into()),
                include: vec![ContextSection::Docs],
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!document_context.entries.is_empty());
    assert!(
        document_context
            .uncertainties
            .iter()
            .all(|text| !text.contains("references are not resolved"))
    );

    std::fs::write(
        worktree.join("overlay_probe.rs"),
        "pub fn overlay_probe() -> u32 { 7 }\n",
    )
    .unwrap();
    ws.git_in(&worktree, &["add", "--", "overlay_probe.rs"]);
    let code = engine
        .open_workspace(
            &caller,
            OpenWorkspaceInput {
                working_directory: Some(worktree.to_string_lossy().into_owned()),
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap();
    let code_search = engine
        .search(
            &caller,
            project_search(
                "handbook",
                "overlay_probe.rs",
                context_target(&code.context_id),
            ),
        )
        .await
        .unwrap();
    assert!(
        code_search
            .hits
            .iter()
            .any(|hit| hit.evidence.layer == ViewLayer::Personal
                && hit.evidence.path.as_str() == "overlay_probe.rs")
    );
    assert!(code_search.gaps.iter().any(|gap| {
        gap.reason == GapReason::NoReferenceResolutionForLanguage
            && gap
                .project
                .as_ref()
                .is_some_and(|project| project.as_str() == "handbook")
            && gap.message.contains("rust")
    }));
    let code_context = engine
        .build_context(
            &caller,
            BuildContextInput {
                target: context_target(&code.context_id),
                task: Some("overlay_probe.rs".into()),
                include: vec![ContextSection::Code],
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!code_context.entries.is_empty());
    assert!(
        code_context
            .uncertainties
            .iter()
            .all(|text| !text.contains("references are not resolved")
                && !text.contains("compiler reference resolution"))
    );
    let overlay_source = code_context
        .entries
        .iter()
        .find(|entry| {
            entry.evidence.as_ref().is_some_and(|evidence| {
                evidence.layer == ViewLayer::Personal
                    && evidence.path.as_str() == "overlay_probe.rs"
            })
        })
        .unwrap();
    assert_eq!(
        overlay_source.content.text(),
        "pub fn overlay_probe() -> u32 { 7 }\n"
    );
    let fetched = engine
        .fetch(
            &caller,
            FetchInput {
                target: context_target(&code.context_id),
                ids: vec![overlay_source.id.clone()],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let fetched = fetched.items.first().unwrap();
    let evidence = overlay_source.evidence.as_ref().unwrap();
    assert_eq!(fetched.evidence.layer, ViewLayer::Personal);
    assert_eq!(fetched.evidence.content_hash, evidence.content_hash);
    assert_eq!(fetched.evidence.commit, evidence.commit);
    assert_eq!(fetched.evidence.lines, evidence.lines);
    assert_eq!(fetched.content.text(), overlay_source.content.text());
    let legacy_context = engine
        .build_context(
            &caller,
            BuildContextInput {
                target: context_target(&code.context_id),
                task: Some("overlay_probe.rs".into()),
                include: vec![ContextSection::Code],
                selection_strategy: Some(ContextSelectionStrategy::Rank),
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!legacy_context.entries.is_empty());
    assert!(
        legacy_context
            .uncertainties
            .iter()
            .any(|text| text.contains("rust") && text.contains("call or test coverage"))
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn search_diagnostics_are_opt_in_and_snippets_preserve_full_fetch_identity() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let path = "src/diagnostic-long.ts";
    let mut source = "export function diagnosticLong(value: number): number {\n".to_owned();
    for ordinal in 0..90 {
        source.push_str(&format!("  value += {ordinal};\n"));
    }
    source.push_str("  return value;\n}\n");
    std::fs::write(ws.project_dir("billing-api").join(path), &source).unwrap();
    ws.commit_all("billing-api", "add synthetic long diagnostic source");
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = context_target(&opened.context_id);
    let mut input = project_search("billing-api", path, target.clone());
    input.kinds = vec![SearchKind::Code];
    let ordinary = engine.search(&caller, input.clone()).await.unwrap();
    assert!(ordinary.diagnostics.is_none());
    assert!(
        serde_json::to_value(&ordinary)
            .unwrap()
            .get("diagnostics")
            .is_none()
    );
    let full = ordinary
        .hits
        .iter()
        .find(|hit| hit.evidence.path.as_str() == path && hit.evidence.lines.line_count() > 40)
        .unwrap();
    let displayed = full.snippet_lines.unwrap();
    assert_eq!(displayed, full.evidence.lines);
    assert!(!full.snippet_truncated);
    let shown = full.snippet.as_ref().unwrap().text();
    assert_eq!(
        u32::try_from(shown.lines().count()).unwrap(),
        displayed.line_count()
    );
    assert!(shown.contains("  return value;"));
    assert!(shown.trim_end().ends_with('}'));

    input.include_diagnostics = Some(true);
    input.include_snippets = Some(false);
    let without_text = engine.search(&caller, input).await.unwrap();
    let same = without_text
        .hits
        .iter()
        .find(|hit| hit.id == full.id)
        .unwrap();
    assert_eq!(same.evidence, full.evidence);
    assert!(same.snippet.is_none() && same.snippet_lines.is_none());
    assert!(!same.snippet_truncated);
    let path_work = without_text.diagnostics.unwrap();
    assert!(path_work.exact_path_embedding_bypassed);
    assert_eq!(path_work.embedding_calls, 0);
    assert!(path_work.embedding.is_none());
    let fetched = engine
        .fetch(
            &caller,
            FetchInput {
                target: target.clone(),
                ids: vec![full.id.clone()],
                context_lines: Some(0),
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(fetched.items.len(), 1);
    assert_eq!(fetched.items[0].evidence.lines, full.evidence.lines);
    assert_eq!(
        fetched.items[0].evidence.content_hash,
        full.evidence.content_hash
    );
    let expected = fetched.items[0].content.text();
    assert_eq!(full.snippet.as_ref().unwrap().text(), expected);
    assert!(fetched.items[0].content.text().lines().count() > 40);

    let mut semantic = project_search("billing-api", "cancel subscription", target);
    semantic.kinds = vec![SearchKind::Code];
    semantic.include_diagnostics = Some(true);
    let measured = engine.search(&caller, semantic).await.unwrap();
    assert!(!measured.hits.is_empty());
    let work = measured.diagnostics.unwrap();
    assert!(work.lexical_queries > 0 && work.lexical_file_hits > 0 && work.lexical_spans > 0);
    assert!(work.semantic_queries > 0 && work.semantic_neighbors > 0);
    assert!(work.embedding_calls > 0);
    assert_eq!(work.embedding_failures, 0);
    assert!(!work.exact_path_embedding_bypassed);
    let usage = work.embedding.unwrap();
    assert!(usage.input_tokens > 0 && usage.requests > 0);
    assert!(
        usage.tokens_estimated,
        "the fake provider has no reported token count"
    );
    assert_eq!(usage.retries, 0);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn bounded_context_reports_only_emitted_fetchable_evidence_and_keeps_unknown_roles() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = context_target(&opened.context_id);
    let mut input = BuildContextInput {
        target: target.clone(),
        task: Some("SubscriptionService.cancelSubscription".to_owned()),
        token_budget: Some(8000),
        include: vec![ContextSection::Code],
        ..BuildContextInput::default()
    };
    let ordinary = engine.build_context(&caller, input.clone()).await.unwrap();
    let ordinary_selection = ordinary.selection.as_ref().unwrap();
    assert_eq!(
        ordinary_selection.strategy,
        ContextSelectionStrategy::Source
    );
    assert!(
        ordinary_selection.considered_candidates > 0
            && ordinary_selection.considered_candidates <= 64
    );
    assert!(ordinary_selection.evaluations > 0 && ordinary_selection.evaluations <= 256);
    assert!(ordinary_selection.covered_roles.is_empty());
    assert!(ordinary_selection.missing_roles.is_empty());
    assert!(ordinary_selection.steps.is_empty());
    assert!(!ordinary.entries.is_empty());
    assert!(ordinary.budget.used <= ordinary.budget.requested);
    for entry in &ordinary.entries {
        assert!(!matches!(
            entry.kind,
            EntryKind::Signature | EntryKind::Skeleton
        ));
        let evidence = entry.evidence.as_ref().unwrap();
        assert_eq!(entry.content_lines, Some(evidence.lines));
        let fetched = engine
            .fetch(
                &caller,
                FetchInput {
                    target: target.clone(),
                    ids: vec![entry.id.clone()],
                    context_lines: Some(0),
                    ..FetchInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(fetched.items.len(), 1);
        let source = fetched.items.first().unwrap();
        assert_eq!(source.evidence.lines, evidence.lines);
        assert_eq!(source.evidence.content_hash, evidence.content_hash);
        assert_eq!(source.evidence.commit, evidence.commit);
        assert_eq!(source.content.text(), entry.content.text());
    }
    input.selection_strategy = Some(ContextSelectionStrategy::BoundedBundles);
    input.desired_roles = vec![
        ContextRole::Entry,
        ContextRole::Config,
        ContextRole::Implementation,
        ContextRole::Caller,
        ContextRole::Callee,
        ContextRole::Surroundings,
    ];
    let packed = engine.build_context(&caller, input).await.unwrap();
    assert!(!packed.entries.is_empty());
    assert!(packed.budget.used <= packed.budget.requested);
    let selection = packed.selection.as_ref().unwrap();
    assert_eq!(selection.strategy, ContextSelectionStrategy::BoundedBundles);
    assert!(selection.considered_candidates > 0 && selection.considered_candidates <= 32);
    assert!(selection.evaluations > 0 && selection.evaluations <= 256);
    assert!(!selection.steps.is_empty());
    for unsupported in [
        ContextRole::Entry,
        ContextRole::Config,
        ContextRole::Caller,
        ContextRole::Callee,
    ] {
        assert!(
            selection.missing_roles.contains(&unsupported),
            "{selection:?}"
        );
        assert!(!selection.covered_roles.contains(&unsupported));
        assert!(!selection.steps.iter().any(|step| step.role == unsupported));
    }
    // This language has import evidence, which must not become a call graph.
    assert!(packed.uncertainties.iter().any(|text| {
        text.contains("shown structural relations do not prove call or test coverage")
    }));
    for step in &selection.steps {
        assert!(!step.source_ids.is_empty());
        assert!(selection.covered_roles.contains(&step.role));
        for id in &step.source_ids {
            let entry = packed.entries.iter().find(|entry| &entry.id == id).unwrap();
            assert!(!matches!(
                entry.kind,
                EntryKind::Signature | EntryKind::Skeleton
            ));
            let evidence = entry.evidence.as_ref().unwrap();
            let fetched = engine
                .fetch(
                    &caller,
                    FetchInput {
                        target: target.clone(),
                        ids: vec![id.clone()],
                        context_lines: Some(0),
                        ..FetchInput::default()
                    },
                )
                .await
                .unwrap();
            assert_eq!(fetched.items.len(), 1);
            assert_eq!(fetched.items[0].evidence.lines, evidence.lines);
            assert_eq!(
                fetched.items[0].evidence.content_hash,
                evidence.content_hash
            );
            assert_eq!(fetched.items[0].evidence.commit, evidence.commit);
            assert_eq!(fetched.items[0].content.text(), entry.content.text());
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn context_section_admission_finds_tests_and_docs_beyond_mixed_source_limit() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    // Isolate admission and lexical refill from embedding quality.
    ws.resolved.projects[0].embedding.provider = None;
    ws.resolved.projects[0].embedding.model = None;
    let root = ws.project_dir("billing-api");
    std::fs::create_dir_all(root.join("src/selection-crowd")).unwrap();
    std::fs::create_dir_all(root.join("test/selection")).unwrap();
    std::fs::create_dir_all(root.join("docs/selection")).unwrap();
    for ordinal in 0..30 {
        std::fs::write(root.join(format!("src/selection-crowd/module-{ordinal:02}.ts")),
            format!("export function selectionadmissionquokka(): string {{\n  return \"selectionadmissionquokka-{ordinal:02}\";\n}}\n")).unwrap();
    }
    std::fs::write(root.join("test/selection/eligible.spec.ts"),
        "// selectionadmissionquokka regression\ndescribe('eligible source', () => {\n  it('retains evidence', () => {});\n});\n").unwrap();
    std::fs::write(root.join("docs/selection/eligible.md"),
        "# Evidence admission\n\nThe selectionadmissionquokka behavior has this source documentation.\n").unwrap();
    ws.commit_all("billing-api", "add synthetic section admission crowding");
    let data = tempfile::tempdir().unwrap();
    // Raise this fixture's project result quota so twenty code decoys truly
    // precede the eligible test and doc. Production defaults remain intact.
    let mut settings = knowell_engine::EngineSettings::default();
    settings.search.fusion.result_quota_per_project = Some(40);
    let embedder = Arc::new(knowell_embed::AnyEmbedder::Fake(
        knowell_embed::FakeEmbedder::new(crate::common::DIMS).unwrap(),
    ));
    let engine = knowell_engine::Engine::builder(
        db.store.clone(),
        crate::common::indexer_config(data.path()),
    )
    .engine_config(&crate::common::engine_config())
    .embedder(name("local"), Arc::clone(&embedder))
    .embedder(name("cloud"), embedder)
    .workspace(ws.resolved.clone())
    .settings(settings)
    .access(Arc::new(crate::common::access()))
    .build()
    .await
    .unwrap();
    let (_, outcomes) = engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    for outcome in outcomes {
        assert!(
            !matches!(outcome, knowell_index::SyncOutcome::Failed { .. }),
            "{outcome:?}"
        );
    }
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = context_target(&opened.context_id);
    let mixed = engine
        .search(
            &caller,
            SearchInput {
                target: target.clone(),
                query: "selectionadmissionquokka".to_owned(),
                limit: Some(20),
                include_snippets: Some(false),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(mixed.hits.len(), 20);
    assert!(
        mixed.hits.iter().all(|hit| hit
            .evidence
            .path
            .as_str()
            .starts_with("src/selection-crowd/")),
        "crowding prerequisite was not exercised: {:?}",
        mixed.hits
    );
    for (section, path) in [
        (ContextSection::Tests, "test/selection/eligible.spec.ts"),
        (ContextSection::Docs, "docs/selection/eligible.md"),
    ] {
        let packed = engine
            .build_context(
                &caller,
                BuildContextInput {
                    target: target.clone(),
                    task: Some("selectionadmissionquokka".to_owned()),
                    token_budget: Some(8000),
                    include: vec![section],
                    selection_strategy: Some(ContextSelectionStrategy::BoundedBundles),
                    ..BuildContextInput::default()
                },
            )
            .await
            .unwrap();
        assert!(
            packed.entries.iter().any(|entry| entry
                .evidence
                .as_ref()
                .is_some_and(|evidence| evidence.path.as_str() == path)),
            "{packed:?}"
        );
        assert!(packed.entries.iter().all(|entry| entry.section == section));
        assert!(packed.budget.used <= packed.budget.requested);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn knowledge_only_context_never_embeds_the_task_or_claims_source_roles() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let data = tempfile::tempdir().unwrap();
    let indexed = indexed_engine(&db, &ws, data.path()).await;
    let budget = knowell_embed::Budget::new(None, None, 0.0).unwrap();
    let embedder = Arc::new(knowell_embed::AnyEmbedder::Fake(
        knowell_embed::FakeEmbedder::new(crate::common::DIMS)
            .unwrap()
            .with_budget(budget.clone()),
    ));
    let engine = knowell_engine::Engine::builder(
        db.store.clone(),
        crate::common::indexer_config(data.path()),
    )
    .engine_config(&crate::common::engine_config())
    .embedder(name("local"), Arc::clone(&embedder))
    .embedder(name("cloud"), embedder)
    .workspace(ws.resolved.clone())
    .access(Arc::new(crate::common::access()))
    .build()
    .await
    .unwrap();
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = context_target(&opened.context_id);
    let mut control = project_search("billing-api", "cancel subscription", target.clone());
    control.kinds = vec![SearchKind::Code];
    control.include_diagnostics = Some(true);
    let measured = engine.search(&caller, control).await.unwrap();
    assert!(measured.diagnostics.unwrap().embedding_calls > 0);
    let spent = budget.spent_tokens();
    assert!(
        spent > 0,
        "positive source-query control must exercise the observed embedder"
    );
    let rest = EngineContext {
        principal: knowell_auth::Principal::User(alice()),
        scopes: None,
        grants: Arc::new({
            let mut grants = GrantSet::new();
            grants.add(
                knowell_auth::Grant::new(
                    knowell_auth::Principal::User(alice()),
                    knowell_auth::Role::Admin,
                    knowell_auth::ResourceScope::Organization,
                )
                .unwrap(),
            );
            grants
        }),
        visible: knowell_auth::ProjectFilter::default(),
        request_id: knowell_auth::RequestId::new("req-knowledge-only").unwrap(),
        audit: Arc::new(knowell_server::MemoryAuditSink::new()),
    };
    for kind in [MemoryKind::Decision, MemoryKind::Rule] {
        let written = engine
            .write_memory(
                &caller,
                WriteMemoryInput {
                    target: target.clone(),
                    scope: MemoryScope {
                        level: ScopeLevel::Project,
                        project: Some(name("billing-api")),
                        task_id: None,
                    },
                    kind,
                    title: format!("knowledgeonlyquokka {kind:?}"),
                    body: "knowledgeonlyquokka preserves the synthetic acceptance decision."
                        .to_owned(),
                    related_symbols: Vec::new(),
                    evidence: Vec::new(),
                    supersedes: None,
                    idempotency_key: None,
                },
            )
            .await
            .unwrap();
        knowell_server::Engine::call(
            &engine,
            &rest,
            EngineRequest::DecideMemory(MemoryDecision {
                id: written.record.id.to_string(),
                action: MemoryAction::Accept,
                note: None,
            }),
        )
        .await
        .unwrap();
    }
    for section in [ContextSection::Memory, ContextSection::Rules] {
        let packed = engine
            .build_context(
                &caller,
                BuildContextInput {
                    target: target.clone(),
                    task: Some("knowledgeonlyquokka".to_owned()),
                    include: vec![section],
                    selection_strategy: Some(ContextSelectionStrategy::BoundedBundles),
                    desired_roles: vec![
                        ContextRole::Entry,
                        ContextRole::Config,
                        ContextRole::Implementation,
                    ],
                    ..BuildContextInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(packed.entries.len(), 1, "{packed:?}");
        assert!(
            packed
                .entries
                .iter()
                .all(|entry| entry.section == section && entry.evidence.is_none())
        );
        let selection = packed.selection.unwrap();
        assert_eq!(selection.considered_candidates, 0);
        assert_eq!(selection.evaluations, 0);
        assert!(selection.steps.is_empty() && selection.covered_roles.is_empty());
        assert_eq!(
            selection.missing_roles,
            [
                ContextRole::Entry,
                ContextRole::Config,
                ContextRole::Implementation
            ]
        );
        assert_eq!(
            budget.spent_tokens(),
            spent,
            "knowledge-only packs must not embed the task"
        );
    }
    let mut memory_search = project_search("billing-api", "knowledgeonlyquokka", target);
    memory_search.kinds = vec![SearchKind::Memory];
    memory_search.include_diagnostics = Some(true);
    let memory = engine.search(&caller, memory_search).await.unwrap();
    assert_eq!(memory.memory_hits.len(), 2);
    let work = memory.diagnostics.unwrap();
    assert_eq!(work.embedding_calls, 0);
    assert!(work.embedding.is_none());
    assert_eq!(work.lexical_queries, 0);
    assert_eq!(work.semantic_queries, 0);
    assert_eq!(budget.spent_tokens(), spent);
    drop(indexed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn project_search_filters_memory_before_limits_and_keeps_shared_scopes() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| matches!(project.name.as_str(), "billing-api" | "storefront-web"));
    for project in &mut ws.resolved.projects {
        project.embedding.provider = None;
        project.embedding.model = None;
    }
    let data = tempfile::tempdir().unwrap();
    let engine = knowell_engine::Engine::builder(
        db.store.clone(),
        crate::common::indexer_config(data.path()),
    )
    .workspace(ws.resolved.clone())
    .access(Arc::new(crate::common::access()))
    .build()
    .await
    .unwrap();
    let registration = engine.add_workspace(&ws.resolved).await.unwrap();
    let billing = registration
        .views
        .iter()
        .find(|view| view.project.as_str() == "billing-api")
        .unwrap()
        .view;
    let storefront = registration
        .views
        .iter()
        .find(|view| view.project.as_str() == "storefront-web")
        .unwrap()
        .view;
    engine
        .indexer()
        .refresh_view(billing, Priority::Interactive)
        .await
        .unwrap();
    engine.indexer().run_until_idle().await.unwrap();
    assert!(
        engine
            .indexer()
            .status(billing)
            .await
            .unwrap()
            .active_generation
            .is_some()
    );
    assert!(
        engine
            .indexer()
            .status(storefront)
            .await
            .unwrap()
            .active_generation
            .is_none()
    );
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let context = context_target(&opened.context_id);
    let mut records = BTreeMap::new();
    // Memory ranking gives no project-scope preference. Timestamps have
    // whole-second precision, so rapid writes can tie and then sort by id.
    for (label, level, project) in [
        ("organization", ScopeLevel::Organization, None),
        ("user", ScopeLevel::User, None),
        ("workspace", ScopeLevel::Workspace, None),
        ("billing", ScopeLevel::Project, Some("billing-api")),
        ("storefront", ScopeLevel::Project, Some("storefront-web")),
    ] {
        let written = engine
            .write_memory(
                &caller,
                WriteMemoryInput {
                    target: context.clone(),
                    scope: MemoryScope {
                        level,
                        project: project.map(name),
                        task_id: None,
                    },
                    kind: MemoryKind::Note,
                    title: format!("memscopequokka {label} decision"),
                    body: format!(
                        "memscopequokka memcrowdingquokka belongs to the synthetic {label} scope."
                    ),
                    related_symbols: Vec::new(),
                    evidence: Vec::new(),
                    supersedes: None,
                    idempotency_key: None,
                },
            )
            .await
            .unwrap();
        records.insert(label, written.record.id);
    }
    // More than the per-word retrieval limit would hide all eligible
    // records if the project filter ran only after fetching the first page.
    for ordinal in 0..50 {
        engine
            .write_memory(
                &caller,
                WriteMemoryInput {
                    target: context.clone(),
                    scope: MemoryScope {
                        level: ScopeLevel::Project,
                        project: Some(name("storefront-web")),
                        task_id: None,
                    },
                    kind: MemoryKind::Note,
                    title: format!("memcrowdingquokka unrelated record {ordinal}"),
                    body: "memcrowdingquokka belongs to the synthetic storefront project."
                        .to_owned(),
                    related_symbols: Vec::new(),
                    evidence: Vec::new(),
                    supersedes: None,
                    idempotency_key: None,
                },
            )
            .await
            .unwrap();
    }
    let stats = engine.indexer().stats();
    let mut crowded_input = project_search("billing-api", "memcrowdingquokka", Target::default());
    crowded_input.kinds = vec![SearchKind::Memory];
    let crowded = engine.search(&caller, crowded_input).await.unwrap();
    let ids: BTreeSet<String> = crowded
        .memory_hits
        .iter()
        .map(|hit| hit.record.id.to_string())
        .collect();
    let expected: BTreeSet<String> = ["organization", "user", "workspace", "billing"]
        .into_iter()
        .map(|label| records.get(label).unwrap().to_string())
        .collect();
    assert_eq!(ids, expected);
    for target in [Target::default(), context.clone()] {
        for (selected, own) in [("billing-api", "billing"), ("storefront-web", "storefront")] {
            let mut input = project_search(selected, "memscopequokka", target.clone());
            input.kinds = vec![SearchKind::Memory];
            let found = engine.search(&caller, input.clone()).await.unwrap();
            let ids: BTreeSet<String> = found
                .memory_hits
                .iter()
                .map(|hit| hit.record.id.to_string())
                .collect();
            let expected: BTreeSet<String> = ["organization", "user", "workspace", own]
                .into_iter()
                .map(|label| records.get(label).unwrap().to_string())
                .collect();
            assert_eq!(ids, expected);
            if selected == "storefront-web" {
                assert!(
                    found
                        .gaps
                        .iter()
                        .any(|gap| gap.reason == GapReason::ProjectNotIndexed)
                );
            }
            // This query matches one word in every eligible record. The
            // ranking contract then uses newest timestamp and lowest id.
            let expected_first = found
                .memory_hits
                .iter()
                .min_by_key(|hit| {
                    (
                        std::cmp::Reverse(
                            time::OffsetDateTime::parse(
                                hit.record.updated_at.as_str(),
                                &time::format_description::well_known::Rfc3339,
                            )
                            .unwrap()
                            .unix_timestamp(),
                        ),
                        hit.record.id.to_string(),
                    )
                })
                .unwrap()
                .record
                .id
                .clone();
            input.limit = Some(1);
            let limited = engine.search(&caller, input).await.unwrap();
            assert_eq!(limited.memory_hits.len(), 1);
            assert_eq!(limited.memory_hits[0].record.id, expected_first);
            assert!(
                limited.memory_hits[0]
                    .record
                    .scope
                    .project
                    .as_ref()
                    .is_none_or(|project| project.as_str() == selected)
            );
        }
    }
    let bob = bob_caller();
    let bob_context = engine
        .open_workspace(&bob, OpenWorkspaceInput::default())
        .await
        .unwrap();
    for target in [Target::default(), context_target(&bob_context.context_id)] {
        let mut input = project_search("billing-api", "memscopequokka", target);
        input.kinds = vec![SearchKind::Memory];
        let found = engine.search(&bob, input).await.unwrap();
        assert_eq!(found.memory_hits.len(), 1);
        assert_eq!(
            &found.memory_hits[0].record.id,
            records.get("billing").unwrap()
        );
    }
    let stolen = engine
        .search(
            &bob,
            project_search("billing-api", "memscopequokka", context.clone()),
        )
        .await
        .unwrap_err();
    assert_eq!(stolen.kind(), "not_found");
    // General read_memory remains independent of the search project filter.
    let all = engine
        .read_memory(
            &caller,
            ReadMemoryInput {
                target: context,
                query: Some("memscopequokka".to_owned()),
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(all.records.len(), 5);
    assert!(
        engine
            .indexer()
            .status(storefront)
            .await
            .unwrap()
            .active_generation
            .is_none()
    );
    assert_eq!(engine.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_search_reports_branch_advancement_without_mutating_the_index() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let old_commit = opened.manifest[0].commit.clone().unwrap();
    let registration = engine.add_workspace(&ws.resolved).await.unwrap();
    let view = registration.views[0].view;
    let before = engine.indexer().status(view).await.unwrap();
    let stats = engine.indexer().stats();
    std::fs::write(
        ws.project_dir("billing-api").join("source-version.txt"),
        "synthetic branch advancement\n",
    )
    .unwrap();
    let new_commit = ws.commit_all("billing-api", "advance synthetic branch without indexing");
    assert_ne!(new_commit, old_commit.as_str());

    let found = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                Target::default(),
            ),
        )
        .await
        .unwrap();
    assert!(!found.hits.is_empty());
    assert!(
        found
            .hits
            .iter()
            .all(|hit| hit.evidence.commit == old_commit
                && hit.evidence.index_state == IndexState::Stale)
    );
    let fresh = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    assert_eq!(fresh.manifest[0].commit.as_ref(), Some(&old_commit));
    assert_eq!(fresh.manifest[0].index_state, IndexState::Stale);
    let pinned = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                context_target(&opened.context_id),
            ),
        )
        .await
        .unwrap();
    assert!(!pinned.hits.is_empty());
    assert!(
        pinned
            .hits
            .iter()
            .all(|hit| hit.evidence.commit == old_commit
                && hit.evidence.index_state == IndexState::Current)
    );
    let after = engine.indexer().status(view).await.unwrap();
    assert_eq!(after.active_generation, before.active_generation);
    assert_eq!(after.active_commit, before.active_commit);
    assert_eq!(after.latest_seen_commit, before.latest_seen_commit);
    assert_eq!(after.building_generation, before.building_generation);
    assert_eq!(engine.indexer().stats(), stats);

    engine
        .indexer()
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    let run = engine.indexer().run_until_idle().await.unwrap();
    assert_eq!(run.failed, 0);
    let updated = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                Target::default(),
            ),
        )
        .await
        .unwrap();
    assert!(!updated.hits.is_empty());
    assert!(
        updated
            .hits
            .iter()
            .all(|hit| hit.evidence.commit.as_str() == new_commit
                && hit.evidence.index_state == IndexState::Current)
    );
    let pinned = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                context_target(&opened.context_id),
            ),
        )
        .await
        .unwrap();
    assert!(!pinned.hits.is_empty());
    assert!(
        pinned
            .hits
            .iter()
            .all(|hit| hit.evidence.commit == old_commit
                && hit.evidence.index_state == IndexState::Current)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fresh_search_rejects_missing_refs_and_preserves_existing_contexts() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| matches!(project.name.as_str(), "billing-api" | "storefront-web"));
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let indexed_commit = opened
        .manifest
        .iter()
        .find(|view| view.project.as_str() == "billing-api")
        .unwrap()
        .commit
        .clone()
        .unwrap();
    let registration = engine.add_workspace(&ws.resolved).await.unwrap();
    let view = registration
        .views
        .iter()
        .find(|view| view.project.as_str() == "billing-api")
        .unwrap()
        .view;
    let before = engine.indexer().status(view).await.unwrap();
    let stats = engine.indexer().stats();
    let found = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                Target::default(),
            ),
        )
        .await
        .unwrap();
    assert!(!found.hits.is_empty());

    ws.git("billing-api", &["update-ref", "-d", "refs/heads/main"]);
    let missing = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                Target::default(),
            ),
        )
        .await
        .unwrap();
    assert!(
        missing.hits.is_empty(),
        "a removed ref must not serve the old active index"
    );
    assert!(missing.gaps.iter().any(|gap| {
        gap.reason == GapReason::RefNotFound && gap.project == Some(name("billing-api"))
    }));

    let fresh = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    assert!(
        fresh
            .manifest
            .iter()
            .all(|view| view.project.as_str() != "billing-api")
    );
    assert!(fresh.gaps.iter().any(|gap| {
        gap.reason == GapReason::RefNotFound && gap.project == Some(name("billing-api"))
    }));
    for target in [Target::default(), context_target(&fresh.context_id)] {
        let healthy = engine
            .search(
                &caller,
                project_search("storefront-web", "createCheckout", target),
            )
            .await
            .unwrap();
        assert!(!healthy.hits.is_empty());
        assert!(
            healthy
                .hits
                .iter()
                .all(|hit| hit.evidence.project.as_str() == "storefront-web")
        );
        assert!(healthy.gaps.iter().all(|gap| {
            gap.project
                .as_ref()
                .is_none_or(|p| p.as_str() == "storefront-web")
        }));
        assert!(
            !healthy
                .gaps
                .iter()
                .any(|gap| gap.reason == GapReason::RefNotFound)
        );
    }

    // A context is an explicit version pin, rather than a new ref lookup.
    let pinned = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                context_target(&opened.context_id),
            ),
        )
        .await
        .unwrap();
    assert!(!pinned.hits.is_empty());
    assert!(
        pinned
            .hits
            .iter()
            .all(|hit| hit.evidence.commit == indexed_commit)
    );
    assert!(
        !pinned
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RefNotFound)
    );
    let after = engine.indexer().status(view).await.unwrap();
    assert_eq!(after.active_generation, before.active_generation);
    assert_eq!(after.active_commit, before.active_commit);
    assert_eq!(after.latest_seen_commit, before.latest_seen_commit);
    assert_eq!(after.building_generation, before.building_generation);
    assert_eq!(
        engine.indexer().stats(),
        stats,
        "search preflight must not index or embed source files"
    );

    // An unrelated operational source failure must not abort a filtered search.
    ws.git(
        "billing-api",
        &["update-ref", "refs/heads/main", indexed_commit.as_str()],
    );
    std::fs::rename(
        ws.project_dir("storefront-web"),
        ws.dir.path().join("unavailable-storefront"),
    )
    .unwrap();
    for caller in [alice_caller(), bob_caller()] {
        let found = engine
            .search(
                &caller,
                project_search(
                    "billing-api",
                    "SubscriptionService.cancelSubscription",
                    Target::default(),
                ),
            )
            .await
            .unwrap();
        assert!(!found.hits.is_empty());
        assert!(found.gaps.iter().all(|gap| {
            gap.project
                .as_ref()
                .is_none_or(|p| p.as_str() == "billing-api")
        }));
    }

    // Invisible and nonexistent selections have the same error class and shape.
    let mut errors = Vec::new();
    for selected in ["storefront-web", "nonexistent"] {
        let error = engine
            .search(
                &bob_caller(),
                project_search(selected, "createCheckout", Target::default()),
            )
            .await
            .unwrap_err();
        assert_eq!(error.kind(), "not_found");
        errors.push(
            error
                .client_message("source-ref-test")
                .replace(selected, "selected"),
        );
    }
    assert_eq!(errors[0], errors[1]);
    assert_eq!(engine.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn requested_commits_must_exist_and_be_commits_even_when_an_index_is_active() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let commit = ws.git("billing-api", &["rev-parse", "HEAD"]);
    let tree = ws.git("billing-api", &["rev-parse", "HEAD^{tree}"]);
    ws.resolved.projects[0].track.value = format!("commit:{commit}").parse().unwrap();
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    assert_eq!(opened.manifest.len(), 1);
    let registration = engine.add_workspace(&ws.resolved).await.unwrap();
    let view = registration.views[0].view;
    let before = engine.indexer().status(view).await.unwrap();
    let stats = engine.indexer().stats();

    for (target, diagnostic) in [
        (
            format!("commit:{}", "f".repeat(commit.len())),
            "does not exist",
        ),
        (format!("commit:{tree}"), "not a commit"),
    ] {
        let requested = Target::workspace(
            name("acme-goods"),
            vec![knowell_mcp::ViewPin {
                project: name("billing-api"),
                view: target.parse().unwrap(),
            }],
        );
        let search = engine
            .search(
                &caller,
                project_search(
                    "billing-api",
                    "SubscriptionService.cancelSubscription",
                    requested,
                ),
            )
            .await
            .unwrap();
        assert!(search.hits.is_empty());
        assert!(search.gaps.iter().any(|gap| {
            gap.reason == GapReason::RefNotFound && gap.message.contains(diagnostic)
        }));
    }
    ws.git(
        "billing-api",
        &["symbolic-ref", "HEAD", "refs/heads/unborn"],
    );
    let worktree = Target::workspace(
        name("acme-goods"),
        vec![knowell_mcp::ViewPin {
            project: name("billing-api"),
            view: "worktree".parse().unwrap(),
        }],
    );
    let unborn = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                worktree,
            ),
        )
        .await
        .unwrap();
    assert!(unborn.hits.is_empty());
    assert!(unborn.gaps.iter().any(|gap| {
        gap.reason == GapReason::RefNotFound && gap.message.contains("has no commits yet")
    }));

    // The synthetic generator creates loose objects. Removing only this
    // commit proves that the already indexed commit view cannot be a fallback.
    let (prefix, suffix) = commit.split_at(2);
    let object = ws
        .project_dir("billing-api")
        .join(".git/objects")
        .join(prefix)
        .join(suffix);
    std::fs::rename(&object, ws.dir.path().join("unavailable-commit-object")).unwrap();
    let missing = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                Target::default(),
            ),
        )
        .await
        .unwrap();
    assert!(missing.hits.is_empty());
    assert!(missing.gaps.iter().any(|gap| {
        gap.reason == GapReason::RefNotFound && gap.message.contains("does not exist")
    }));
    let after = engine.indexer().status(view).await.unwrap();
    assert_eq!(after.active_generation, before.active_generation);
    assert_eq!(after.active_commit, before.active_commit);
    assert_eq!(after.building_generation, before.building_generation);
    assert_eq!(engine.indexer().stats(), stats);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn directory_source_remains_pinned_without_claiming_commit_evidence() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    std::fs::rename(
        ws.project_dir("billing-api").join(".git"),
        ws.dir.path().join("saved-git-directory"),
    )
    .unwrap();
    ws.resolved.projects[0].track.value = "worktree".parse().unwrap();
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    assert_eq!(opened.manifest.len(), 1);
    assert!(opened.manifest[0].commit.is_none());
    assert!(
        !opened
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RefNotFound)
    );
    let search = engine
        .search(
            &caller,
            project_search(
                "billing-api",
                "SubscriptionService.cancelSubscription",
                Target::default(),
            ),
        )
        .await
        .unwrap();
    assert!(search.hits.is_empty());
    assert!(
        search
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::ExcludedByPolicy)
    );
    assert!(
        !search
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RefNotFound)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_postgres_serves_lexical_symbols_graph_and_memory() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let Some(db) =
        crate::common::TestDb::create_from(module_path!(), crate::common::PLAIN_ENV).await
    else {
        return;
    };
    assert!(db.store.check_server().await.unwrap().vector.is_none());
    let ws = fixture_workspace();
    let data = tempfile::tempdir().unwrap();
    // Providers remain configured: storage availability must prevent T2
    // without blocking the rest of indexing or sending any inputs.
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    assert_eq!(opened.manifest.len(), 10);
    assert!(opened.manifest.iter().all(|view| view.commit.is_some()));
    let target = context_target(&opened.context_id);
    let found = engine
        .search(
            &caller,
            SearchInput {
                target: target.clone(),
                query: "cancel subscription".into(),
                limit: Some(40),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!found.hits.is_empty());
    assert!(found.hits.iter().any(|hit| {
        hit.evidence
            .why
            .iter()
            .any(|why| matches!(why, MatchReason::Lexical { .. }))
    }));
    assert!(
        found
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::EmbeddingsNotReady
                && gap.message.contains("pgvector")),
        "{:?}",
        found.gaps
    );

    let inspected = engine
        .inspect_symbol(
            &caller,
            InspectSymbolInput {
                target: target.clone(),
                symbol: SymbolRef {
                    id: None,
                    symbol: Some("SubscriptionService.cancelSubscription".into()),
                    project: Some(name("billing-api")),
                },
                include: Vec::new(),
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(inspected.symbols.len(), 1);
    let symbol = &inspected.symbols[0];
    assert!(!symbol.references.is_empty());
    let trace = engine
        .trace_flow(
            &caller,
            TraceFlowInput {
                target: target.clone(),
                symbol: Some("SubscriptionService".into()),
                project: Some(name("billing-api")),
                direction: Some(knowell_mcp::tools::FlowDirection::Upstream),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!trace.nodes.is_empty());
    assert!(
        !trace
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RelationsNotReady)
    );

    let written = engine
        .write_memory(
            &caller,
            WriteMemoryInput {
                target: target.clone(),
                scope: MemoryScope {
                    level: ScopeLevel::Project,
                    project: Some(name("billing-api")),
                    task_id: None,
                },
                kind: MemoryKind::Decision,
                title: "Cancellation keeps benefits until the period ends".into(),
                body: "The subscription service preserves benefits until period end.".into(),
                related_symbols: vec!["SubscriptionService.cancelSubscription".into()],
                evidence: vec![symbol.id.clone()],
                supersedes: None,
                idempotency_key: Some("plain-postgres-decision".into()),
            },
        )
        .await
        .unwrap();
    assert!(written.created);
    let memory = engine
        .read_memory(
            &caller,
            ReadMemoryInput {
                target,
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(memory.records.len(), 1);
    assert_eq!(memory.records[0].id, written.record.id);
    assert_eq!(memory.records[0].evidence.len(), 1);
    assert_eq!(engine.indexer().stats().embedding_calls, 0);
    let mut conn = db.store.acquire().await.unwrap();
    assert!(
        !knowell_store::embeddings::available(&mut conn)
            .await
            .unwrap()
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn engine_serves_every_tool_over_the_indexed_fixture() {
    if !git_available() {
        eprintln!("skipping: git is not installed");
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    // One local-only project names a cloud provider: nothing may be sent.
    for project in &mut ws.resolved.projects {
        if project.name.as_str() == "handbook"
            && let Some(provider) = project.embedding.provider.as_mut()
        {
            provider.value = name("cloud");
        }
    }
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let agent_a = alice_caller();
    let agent_b = bob_caller();

    // ---------------------------------------------------------- open_workspace
    let opened = engine
        .open_workspace(&agent_a, OpenWorkspaceInput::default())
        .await
        .unwrap();
    assert_eq!(opened.workspace.as_str(), "acme-goods");
    assert_eq!(opened.manifest.len(), 10, "{:?}", opened.manifest);
    for view in &opened.manifest {
        assert!(view.commit.is_some(), "{view:?}");
        assert_eq!(view.layer, ViewLayer::Shared);
    }
    assert_eq!(opened.projects.len(), 10);
    let billing = opened
        .projects
        .iter()
        .find(|p| p.name.as_str() == "billing-api")
        .unwrap();
    assert!(
        billing.languages.contains(&"typescript".to_owned()),
        "{billing:?}"
    );
    let ctx = context_target(&opened.context_id);

    // ------------------------------------------------------------------ search
    let found = engine
        .search(
            &agent_a,
            SearchInput {
                target: ctx.clone(),
                query: "cancel subscription".into(),
                limit: Some(40),
                include_diagnostics: Some(true),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    let projects: BTreeSet<&str> = found
        .hits
        .iter()
        .map(|h| h.evidence.project.as_str())
        .collect();
    eprintln!(
        "search hits: {:#?}",
        found
            .hits
            .iter()
            .map(|h| (
                h.evidence.project.as_str(),
                h.evidence.path.as_str(),
                h.title.as_str()
            ))
            .collect::<Vec<_>>()
    );
    for expected in ["billing-api", "storefront-web", "mobile-app"] {
        assert!(
            projects.contains(expected),
            "{expected} missing from {projects:?}"
        );
    }
    for hit in &found.hits {
        let e = &hit.evidence;
        assert_eq!(e.commit.as_str().len(), 40);
        assert!(e.lines.start() >= 1 && e.lines.end() >= e.lines.start());
        assert!(!e.why.is_empty(), "{hit:?}");
        assert!(hit.id.as_str().starts_with("kn:"));
        assert!(hit.snippet.is_some());
    }
    assert!(found.hits.iter().any(|h| {
        h.evidence
            .why
            .iter()
            .any(|w| matches!(w, MatchReason::Lexical { .. }))
    }));
    let diagnostics = found.diagnostics.as_ref().unwrap();
    assert!(diagnostics.embedding_calls > 0 && diagnostics.semantic_neighbors > 0);
    // Adaptive fusion and complementary Source packing may choose only lexical
    // results for the mixed query above. A query absent from the fixture proves
    // that vector candidates reach the output, without claiming that this fake
    // embedder can judge semantic relevance.
    let ranked = engine
        .search(
            &agent_a,
            SearchInput {
                target: ctx.clone(),
                query: "quartz nebula".into(),
                limit: Some(40),
                include_snippets: Some(false),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        ranked
            .hits
            .iter()
            .any(|h| h.evidence.freshness == FreshnessTier::T2Embeddings
                && h.evidence
                    .why
                    .iter()
                    .any(|reason| matches!(reason, MatchReason::Semantic { .. }))),
        "the semantic source should contribute"
    );
    assert!(ranked.hits.iter().all(|hit| {
        !hit.evidence.why.iter().any(|reason| {
            matches!(
                reason,
                MatchReason::Lexical { .. }
                    | MatchReason::ExactSymbol { .. }
                    | MatchReason::ExactPath
            )
        })
    }));
    assert!(
        found
            .gaps
            .iter()
            .any(|g| g.reason == GapReason::EmbeddingsNotReady
                && g.message.contains("handbook")
                && g.message.contains("nothing was sent")),
        "{:?}",
        found.gaps
    );

    // ------------------------------------------------------------------- fetch
    let first = found.hits.first().unwrap();
    let fetched = engine
        .fetch(
            &agent_a,
            FetchInput {
                target: ctx.clone(),
                ids: vec![first.id.clone()],
                paths: vec![FileLocator {
                    project: name("billing-api"),
                    path: knowell_core::RepoPath::new("src/subscriptions/subscription.service.ts")
                        .unwrap(),
                    lines: None,
                }],
                context_lines: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(fetched.items.len(), 2, "{:?}", fetched.gaps);
    assert!(
        fetched.items[1]
            .content
            .text()
            .contains("cancelSubscription")
    );
    assert_eq!(
        fetched.items[0].evidence.content_hash,
        first.evidence.content_hash
    );

    // ---------------------------------------------------------- inspect_symbol
    let inspected = engine
        .inspect_symbol(
            &agent_a,
            InspectSymbolInput {
                target: ctx.clone(),
                symbol: SymbolRef {
                    id: None,
                    symbol: Some("SubscriptionService.cancelSubscription".into()),
                    project: None,
                },
                include: Vec::new(),
                limit: None,
            },
        )
        .await
        .unwrap();
    assert_eq!(inspected.symbols.len(), 1, "{inspected:?}");
    let symbol = &inspected.symbols[0];
    assert_eq!(
        symbol.qualified_name,
        "SubscriptionService.cancelSubscription"
    );
    assert_eq!(symbol.definition.project.as_str(), "billing-api");
    assert_eq!(
        symbol.definition.path.as_str(),
        "src/subscriptions/subscription.service.ts"
    );
    assert!(
        symbol
            .signature
            .as_ref()
            .unwrap()
            .text()
            .contains("cancelSubscription")
    );
    assert!(
        symbol
            .doc
            .as_ref()
            .is_some_and(|d| d.text().contains("ADR-0005"))
    );
    assert!(
        !symbol.references.is_empty(),
        "importers of the service file"
    );
    assert!(
        inspected
            .gaps
            .iter()
            .any(|g| g.reason == GapReason::NoReferenceResolutionForLanguage)
    );

    // ----------------------------------------------------------- build_context
    let pack = engine
        .build_context(
            &agent_a,
            BuildContextInput {
                target: ctx.clone(),
                task: Some("let members cancel their subscription at period end".into()),
                token_budget: Some(1500),
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!pack.entries.is_empty());
    assert!(
        pack.budget.used <= pack.budget.requested,
        "{:?}",
        pack.budget
    );
    let total: u32 = pack.entries.iter().map(|e| e.estimated_tokens).sum();
    assert!(total <= 1500, "{total}");
    for entry in &pack.entries {
        let evidence = entry
            .evidence
            .as_ref()
            .expect("code entries cite their source");
        assert_eq!(evidence.commit.as_str().len(), 40);
        assert!(!entry.why_relevant.is_empty());
    }

    // ------------------------------------------------------------ index_status
    let status = engine
        .index_status(
            &agent_a,
            IndexStatusInput {
                target: ctx.clone(),
                ..IndexStatusInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(status.projects.len(), 10);
    let billing_status = status
        .projects
        .iter()
        .find(|p| p.project.as_str() == "billing-api")
        .unwrap();
    assert_eq!(billing_status.tiers.len(), 4);
    assert!(
        billing_status
            .tiers
            .iter()
            .take(3)
            .all(|t| t.state == TierState::Ready),
        "{:?}",
        billing_status.tiers
    );
    assert_eq!(
        billing_status.indexed_commit,
        billing_status.latest_seen_commit
    );
    let handbook_status = status
        .projects
        .iter()
        .find(|p| p.project.as_str() == "handbook")
        .unwrap();
    assert_eq!(handbook_status.tiers[2].state, TierState::Unavailable);

    // --------------------------------------------------- trace / impact / etc.
    let trace = engine
        .trace_flow(
            &agent_a,
            TraceFlowInput {
                target: ctx.clone(),
                symbol: Some("SubscriptionService".into()),
                project: Some(name("billing-api")),
                direction: Some(knowell_mcp::tools::FlowDirection::Upstream),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!trace.nodes.is_empty());
    // knowell-link runs as the indexer's T3 relation stage, so relations are
    // ready and the trace must not claim otherwise.
    assert!(
        !trace
            .gaps
            .iter()
            .any(|g| g.reason == GapReason::RelationsNotReady),
        "{:?}",
        trace.gaps
    );

    let patch = "--- a/src/subscriptions/subscription.service.ts\n+++ b/src/subscriptions/subscription.service.ts\n@@ -1,1 +1,2 @@\n+// reviewed\n";
    let base_first_line = fetched.items[1]
        .content
        .text()
        .lines()
        .next()
        .unwrap()
        .to_owned();
    let patch = format!("{patch} {base_first_line}\n");
    let impact = engine
        .analyze_impact(
            &agent_a,
            AnalyzeImpactInput {
                target: ctx.clone(),
                change: Some(ChangeSubject::Patch {
                    project: name("billing-api"),
                    patch,
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!impact.changed.is_empty(), "{impact:?}");
    assert!(impact.risk.is_some());
    let symbol_impact = engine
        .analyze_impact(
            &agent_a,
            AnalyzeImpactInput {
                target: ctx.clone(),
                change: Some(ChangeSubject::Symbol {
                    symbol: SymbolRef {
                        id: None,
                        symbol: Some("SubscriptionService.cancelSubscription".into()),
                        project: Some(name("billing-api")),
                    },
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        !symbol_impact.impacted.is_empty(),
        "importers are affected: {symbol_impact:?}"
    );
    assert!(
        symbol_impact
            .impacted
            .iter()
            .any(|i| i.evidence.path.as_str() == "src/subscriptions/subscriptions.controller.ts"),
        "{symbol_impact:?}"
    );
    assert!(
        trace
            .nodes
            .iter()
            .any(|n| n.label.contains("subscriptions.controller")
                || n.label.starts_with("SubscriptionsController")),
        "{trace:?}"
    );

    let contracts = engine
        .contracts(
            &agent_a,
            ContractsInput {
                target: ctx.clone(),
                query: Some("POST /v1/subscriptions/{id}/cancel".into()),
                ..ContractsInput::default()
            },
        )
        .await
        .unwrap();
    // Contracts are extracted by the T3 relation stage.
    assert!(
        !contracts
            .gaps
            .iter()
            .any(|g| g.message.starts_with("contracts_not_extracted")),
        "{:?}",
        contracts.gaps
    );
    assert!(!contracts.contracts.is_empty(), "{contracts:?}");

    let history = engine
        .history(
            &agent_a,
            HistoryInput {
                target: ctx.clone(),
                project: Some(name("billing-api")),
                path: Some(
                    knowell_core::RepoPath::new("src/subscriptions/subscription.service.ts")
                        .unwrap(),
                ),
                ..HistoryInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!history.gaps.is_empty());

    // ------------------------------------------------------------ write_memory
    let written = engine
        .write_memory(
            &agent_a,
            WriteMemoryInput {
                target: ctx.clone(),
                scope: MemoryScope {
                    level: ScopeLevel::Project,
                    project: Some(name("billing-api")),
                    task_id: None,
                },
                kind: MemoryKind::Decision,
                title: "Cancellation keeps benefits until the period ends".into(),
                body: "cancelSubscription marks the row cancelled but keeps benefits (ADR-0005).".into(),
                related_symbols: vec!["src/subscriptions/subscription.service.ts#SubscriptionService.cancelSubscription".into()],
                evidence: vec![symbol.id.clone()],
                supersedes: None,
                idempotency_key: Some("decision-1".into()),
            },
        )
        .await
        .unwrap();
    assert!(written.created);
    assert_eq!(
        written.record.status,
        MemoryStatus::Proposed,
        "agents only propose"
    );
    assert_eq!(written.record.evidence.len(), 1);
    let again = engine
        .write_memory(
            &agent_a,
            WriteMemoryInput {
                target: ctx.clone(),
                scope: MemoryScope {
                    level: ScopeLevel::Project,
                    project: Some(name("billing-api")),
                    task_id: None,
                },
                kind: MemoryKind::Decision,
                title: "Cancellation keeps benefits until the period ends".into(),
                body: "same".into(),
                related_symbols: Vec::new(),
                evidence: Vec::new(),
                supersedes: None,
                idempotency_key: Some("decision-1".into()),
            },
        )
        .await
        .unwrap();
    assert!(!again.created);
    assert_eq!(again.record.id, written.record.id);
    let token = format!("ghp_{}", "FAKE".repeat(9));
    let secret = engine
        .write_memory(
            &agent_a,
            WriteMemoryInput {
                target: ctx.clone(),
                scope: MemoryScope {
                    level: ScopeLevel::Workspace,
                    project: None,
                    task_id: None,
                },
                kind: MemoryKind::Note,
                title: "deploy token".into(),
                body: format!("use {token} for deploys"),
                related_symbols: Vec::new(),
                evidence: Vec::new(),
                supersedes: None,
                idempotency_key: None,
            },
        )
        .await
        .unwrap_err();
    assert_eq!(secret.kind(), "invalid_input");
    let shown = format!("{secret} {}", secret.client_message("r"));
    assert!(
        !shown.contains(&token) && !shown.contains("FAKEFAKE"),
        "{shown}"
    );
    let memory = engine
        .read_memory(
            &agent_a,
            ReadMemoryInput {
                target: ctx.clone(),
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(memory.records.len(), 1);
    assert_eq!(memory.records[0].status, MemoryStatus::Proposed);

    // ------------------------------------------------- REST: accept by a human
    let rest_context = EngineContext {
        principal: knowell_auth::Principal::User(alice()),
        scopes: None,
        grants: Arc::new({
            let mut g = GrantSet::new();
            g.add(
                knowell_auth::Grant::new(
                    knowell_auth::Principal::User(alice()),
                    knowell_auth::Role::Admin,
                    knowell_auth::ResourceScope::Organization,
                )
                .unwrap(),
            );
            g
        }),
        visible: knowell_auth::ProjectFilter::default(),
        request_id: knowell_auth::RequestId::new("req-1").unwrap(),
        audit: Arc::new(knowell_server::MemoryAuditSink::new()),
    };
    let listed = knowell_server::Engine::call(&engine, &rest_context, EngineRequest::Memory)
        .await
        .unwrap();
    assert_eq!(listed.as_array().unwrap().len(), 1);
    assert_eq!(listed[0]["state"], "proposed");
    assert_eq!(listed[0]["kind"], "agent-finding");
    let accepted = knowell_server::Engine::call(
        &engine,
        &rest_context,
        EngineRequest::DecideMemory(MemoryDecision {
            id: written.record.id.to_string(),
            action: MemoryAction::Accept,
            note: None,
        }),
    )
    .await
    .unwrap();
    assert_eq!(accepted["state"], "accepted");
    let search_rest = knowell_server::Engine::call(
        &engine,
        &rest_context,
        EngineRequest::Search(
            serde_json::from_value(serde_json::json!({
                "query": "cancel subscription", "limit": 5
            }))
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    assert!(!search_rest["results"].as_array().unwrap().is_empty());
    for key in ["queryClass", "tookMs", "skipped", "tokensReturned"] {
        assert!(search_rest.get(key).is_some(), "{key}");
    }
    let profiles = knowell_server::Engine::call(&engine, &rest_context, EngineRequest::Profiles)
        .await
        .unwrap();
    assert_eq!(profiles.as_array().unwrap().len(), 1, "{profiles}");
    let profile_id = profiles[0]["id"].as_str().unwrap().to_owned();
    let estimate = knowell_server::Engine::call(
        &engine,
        &rest_context,
        EngineRequest::SwitchEstimate {
            to_profile_id: profile_id,
        },
    )
    .await
    .unwrap();
    assert!(estimate["warnings"].is_array());
    let graph = knowell_server::Engine::call(
        &engine,
        &rest_context,
        EngineRequest::Graph(
            serde_json::from_value(serde_json::json!({"mode": "hierarchy"})).unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(graph["nodes"].as_array().unwrap().len(), 10);
    for request in [
        EngineRequest::HealthDetail,
        EngineRequest::GraphInsights,
        EngineRequest::Domains,
        EngineRequest::Glossary,
        EngineRequest::Tasks,
        EngineRequest::Rules,
        EngineRequest::EvalReports,
        EngineRequest::Usage { days: 7 },
        EngineRequest::Integrations,
        EngineRequest::Admin,
    ] {
        knowell_server::Engine::call(&engine, &rest_context, request)
            .await
            .unwrap();
    }

    let usage =
        knowell_server::Engine::call(&engine, &rest_context, EngineRequest::Usage { days: 7 })
            .await
            .unwrap();
    let search_usage = usage["tools"]
        .as_array()
        .unwrap()
        .iter()
        .find(|t| t["tool"] == "search")
        .unwrap();
    assert!(search_usage["calls"].as_u64().unwrap() >= 1);
    assert!(usage["agents"][0]["agent"].is_string());

    // ------------------------------------------------------------- permissions
    // REST for a user who may only read billing-api.
    let bob_rest = EngineContext {
        principal: knowell_auth::Principal::User(crate::common::bob()),
        scopes: None,
        grants: Arc::new({
            let mut g = GrantSet::new();
            g.add(
                knowell_auth::Grant::new(
                    knowell_auth::Principal::User(crate::common::bob()),
                    knowell_auth::Role::Member,
                    knowell_auth::ResourceScope::project(name("acme-goods"), name("billing-api")),
                )
                .unwrap(),
            );
            g
        }),
        visible: knowell_auth::ProjectFilter::default(),
        request_id: knowell_auth::RequestId::new("req-2").unwrap(),
        audit: Arc::new(knowell_server::MemoryAuditSink::new()),
    };
    let bob_rest_search = knowell_server::Engine::call(
        &engine,
        &bob_rest,
        EngineRequest::Search(
            serde_json::from_value(
                serde_json::json!({"query": "cancel subscription", "limit": 50}),
            )
            .unwrap(),
        ),
    )
    .await
    .unwrap();
    let names: BTreeSet<&str> = bob_rest_search["results"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|r| r["projectName"].as_str())
        .collect();
    assert_eq!(names.into_iter().collect::<Vec<_>>(), ["billing-api"]);
    let bob_graph = knowell_server::Engine::call(
        &engine,
        &bob_rest,
        EngineRequest::Graph(
            serde_json::from_value(serde_json::json!({"mode": "hierarchy"})).unwrap(),
        ),
    )
    .await
    .unwrap();
    assert_eq!(bob_graph["nodes"].as_array().unwrap().len(), 1);
    let web_id = graph["nodes"]
        .as_array()
        .unwrap()
        .iter()
        .find(|n| n["label"] == "storefront-web")
        .and_then(|n| n["projectId"].as_str())
        .unwrap()
        .to_owned();
    let denied = knowell_server::Engine::call(
        &engine,
        &bob_rest,
        EngineRequest::Search(
            serde_json::from_value(serde_json::json!({"query": "cancel", "projectIds": [web_id]}))
                .unwrap(),
        ),
    )
    .await
    .unwrap_err();
    assert!(matches!(
        denied,
        knowell_server::EngineError::NotFound { .. }
    ));
    let bob_decide = knowell_server::Engine::call(
        &engine,
        &bob_rest,
        EngineRequest::DecideMemory(MemoryDecision {
            id: written.record.id.to_string(),
            action: MemoryAction::Reject,
            note: None,
        }),
    )
    .await
    .unwrap_err();
    assert!(
        matches!(bob_decide, knowell_server::EngineError::Forbidden { .. }),
        "a member may not review memory: {bob_decide:?}"
    );

    let bob_open = engine
        .open_workspace(&agent_b, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let bob_projects: Vec<&str> = bob_open
        .manifest
        .iter()
        .map(|v| v.project.as_str())
        .collect();
    assert_eq!(bob_projects, ["billing-api"]);
    assert_eq!(bob_open.projects.len(), 1);
    let bob_ctx = context_target(&bob_open.context_id);
    let bob_search = engine
        .search(
            &agent_b,
            SearchInput {
                target: bob_ctx.clone(),
                query: "cancel subscription".into(),
                limit: Some(50),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(!bob_search.hits.is_empty());
    assert!(
        bob_search
            .hits
            .iter()
            .all(|h| h.evidence.project.as_str() == "billing-api")
    );
    assert!(bob_search.memory_hits.iter().all(|m| {
        m.record
            .scope
            .project
            .as_ref()
            .is_none_or(|p| p.as_str() == "billing-api")
    }));
    let bob_symbols = engine
        .inspect_symbol(
            &agent_b,
            InspectSymbolInput {
                target: bob_ctx.clone(),
                symbol: SymbolRef {
                    id: None,
                    symbol: Some("cancelSubscription".into()),
                    project: None,
                },
                include: Vec::new(),
                limit: None,
            },
        )
        .await
        .unwrap();
    assert!(
        bob_symbols
            .symbols
            .iter()
            .all(|s| s.definition.project.as_str() == "billing-api")
    );
    let web_hit = found
        .hits
        .iter()
        .find(|h| h.evidence.project.as_str() == "storefront-web")
        .unwrap();
    let bob_fetch = engine
        .fetch(
            &agent_b,
            FetchInput {
                target: bob_ctx.clone(),
                ids: vec![web_hit.id.clone()],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(bob_fetch.items.is_empty());
    assert!(
        bob_fetch
            .gaps
            .iter()
            .all(|g| g.reason == GapReason::NotFound)
    );
    let bob_pack = engine
        .build_context(
            &agent_b,
            BuildContextInput {
                target: bob_ctx.clone(),
                task: Some("cancel subscription in the web and mobile clients".into()),
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        bob_pack
            .entries
            .iter()
            .filter_map(|e| e.evidence.as_ref())
            .all(|e| e.project.as_str() == "billing-api")
    );
    let bob_status = engine
        .index_status(
            &agent_b,
            IndexStatusInput {
                target: bob_ctx.clone(),
                ..IndexStatusInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(bob_status.projects.len(), 1);
    let bob_trace = engine
        .trace_flow(
            &agent_b,
            TraceFlowInput {
                target: bob_ctx.clone(),
                symbol: Some("cancelSubscription".into()),
                direction: Some(knowell_mcp::tools::FlowDirection::Both),
                ..TraceFlowInput::default()
            },
        )
        .await
        .unwrap();
    assert!(bob_trace.nodes.iter().all(|n| {
        n.project
            .as_ref()
            .is_none_or(|p| p.as_str() == "billing-api")
    }));
    let bob_impact = engine
        .analyze_impact(
            &agent_b,
            AnalyzeImpactInput {
                target: bob_ctx.clone(),
                change: Some(ChangeSubject::Symbol {
                    symbol: SymbolRef {
                        id: None,
                        symbol: Some("cancelSubscription".into()),
                        project: None,
                    },
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        bob_impact
            .changed
            .iter()
            .chain(bob_impact.impacted.iter())
            .chain(bob_impact.tests.iter())
            .all(|i| i.evidence.project.as_str() == "billing-api")
    );
    let bob_memory = engine
        .read_memory(
            &agent_b,
            ReadMemoryInput {
                target: bob_ctx.clone(),
                ..ReadMemoryInput::default()
            },
        )
        .await
        .unwrap();
    assert!(bob_memory.records.iter().all(|r| {
        r.scope
            .project
            .as_ref()
            .is_some_and(|p| p.as_str() == "billing-api")
    }));
    let bob_contracts = engine
        .contracts(
            &agent_b,
            ContractsInput {
                target: bob_ctx.clone(),
                ..ContractsInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        bob_contracts
            .contracts
            .iter()
            .flat_map(|c| c.participants.iter())
            .all(|p| p.project.as_str() == "billing-api")
    );
    // Another identity's context is unknown, and so are invisible projects.
    let stolen = engine
        .search(
            &agent_b,
            SearchInput {
                target: ctx.clone(),
                query: "cancel".into(),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(stolen.kind(), "not_found");
    let invisible = engine
        .open_workspace(
            &agent_b,
            OpenWorkspaceInput {
                views: vec![knowell_mcp::ViewPin {
                    project: name("storefront-web"),
                    view: "branch:main".parse().unwrap(),
                }],
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap_err();
    assert_eq!(invisible.kind(), "not_found");

    // ------------------------------------------------------- personal overlay
    let worktree = ws.dir.path().join("billing-wt");
    ws.git(
        "billing-api",
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/grace",
            worktree.to_str().unwrap(),
        ],
    );
    let service = worktree.join("src/subscriptions/subscription.service.ts");
    let text = std::fs::read_to_string(&service).unwrap();
    std::fs::write(
        &service,
        text.replace(
            "async cancelSubscription(",
            "async cancelWithGracePeriodQuokka(): Promise<void> {}\n\n  async cancelSubscription(",
        ),
    )
    .unwrap();
    let personal = engine
        .open_workspace(
            &agent_a,
            OpenWorkspaceInput {
                working_directory: Some(worktree.to_string_lossy().into_owned()),
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(
        personal.current_project.as_ref().map(|p| p.as_str()),
        Some("billing-api")
    );
    let billing_view = personal
        .manifest
        .iter()
        .find(|v| v.project.as_str() == "billing-api")
        .unwrap();
    assert_eq!(
        billing_view.layer,
        ViewLayer::Personal,
        "{:?}",
        personal.gaps
    );
    let personal_hits = engine
        .search(
            &agent_a,
            SearchInput {
                target: context_target(&personal.context_id),
                query: "cancelWithGracePeriodQuokka".into(),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    let hit = personal_hits
        .hits
        .first()
        .expect("the overlay symbol is found");
    assert_eq!(hit.evidence.layer, ViewLayer::Personal);
    assert!(
        personal_hits
            .hits
            .iter()
            .filter(|h| h.evidence.path.as_str() == "src/subscriptions/subscription.service.ts")
            .all(|h| h.evidence.layer == ViewLayer::Personal)
    );
    // Bob shares the project but not Alice's worktree.
    let bob_overlay = engine
        .search(
            &agent_b,
            SearchInput {
                target: bob_ctx.clone(),
                query: "cancelWithGracePeriodQuokka".into(),
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        bob_overlay
            .hits
            .iter()
            .all(|h| h.evidence.layer == ViewLayer::Shared && !h.title.contains("Quokka"))
    );

    // ------------------------------------------------ resume after a commit
    let saved = engine
        .save_checkpoint(
            &agent_a,
            SaveCheckpointInput {
                target: ctx.clone(),
                goal: Some("Add a grace period to subscription cancellation".into()),
                progress: "Read SubscriptionService.cancelSubscription".into(),
                next_steps: vec!["change the service".into()],
                open_questions: vec!["does mobile need a new string?".into()],
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap();
    assert!(saved.created_task && saved.created);
    let commit_file = ws
        .project_dir("billing-api")
        .join("src/subscriptions/subscription.service.ts");
    let original = std::fs::read_to_string(&commit_file).unwrap();
    std::fs::write(
        &commit_file,
        format!("{original}\n// grace period follows ADR-0005\n"),
    )
    .unwrap();
    let new_commit = ws.commit_all("billing-api", "note the grace period");
    engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let reopened = engine
        .open_workspace(&agent_a, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let reopened_billing = reopened
        .manifest
        .iter()
        .find(|v| v.project.as_str() == "billing-api")
        .unwrap();
    assert_eq!(
        reopened_billing.commit.as_ref().unwrap().as_str(),
        new_commit
    );
    let resumed = engine
        .resume_task(
            &agent_a,
            ResumeTaskInput {
                target: context_target(&reopened.context_id),
                task_id: Some(saved.task_id.clone()),
                ..ResumeTaskInput::default()
            },
        )
        .await
        .unwrap();
    let task = resumed.task.expect("the task resumes");
    assert!(
        task.changed_since
            .iter()
            .any(|c| c.project.as_str() == "billing-api"
                && c.path.as_str() == "src/subscriptions/subscription.service.ts"
                && c.to_commit.as_str() == new_commit),
        "{:?}",
        task.changed_since
    );
    assert_eq!(task.next_steps.len(), 1);
    assert_eq!(task.open_questions.len(), 1);
    let listed_tasks = engine
        .resume_task(
            &agent_a,
            ResumeTaskInput {
                target: context_target(&reopened.context_id),
                ..ResumeTaskInput::default()
            },
        )
        .await
        .unwrap();
    assert_eq!(listed_tasks.tasks.len(), 1);
    // The old context still pins the old commit.
    let old_view = engine
        .index_status(
            &agent_a,
            IndexStatusInput {
                target: ctx.clone(),
                projects: vec![name("billing-api")],
                ..IndexStatusInput::default()
            },
        )
        .await
        .unwrap();
    assert_ne!(
        old_view.projects[0]
            .indexed_commit
            .as_ref()
            .unwrap()
            .as_str(),
        new_commit
    );

    // ------------------------------------------------------- evaluation hook
    let access = Access::new(
        knowell_auth::Principal::User(alice()),
        rest_context.grants.clone(),
    );
    let hybrid = HybridRetriever::new(&engine, &access, &name("acme-goods"))
        .await
        .unwrap();
    // Earlier in this test a local-only project deliberately named a cloud
    // provider. Evaluation must report that incomplete semantic coverage.
    let error = hybrid
        .search("how do we cancel a subscription", 10)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("every requested search component"),
        "{error}"
    );

    // Preserve the positive evaluation coverage on a fully indexed fixture.
    let complete_ws = fixture_workspace();
    let complete_db = require_db!();
    let complete_engine = indexed_engine(
        &complete_db,
        &complete_ws,
        &complete_ws.dir.path().join("eval-data"),
    )
    .await;
    let hybrid = HybridRetriever::new(&complete_engine, &access, &name("acme-goods"))
        .await
        .unwrap();
    let measured_root = tempfile::tempdir().unwrap();
    let original_root = measured_root.path().join("ws");
    ws.fixture
        .write_to(&original_root, &knowell_eval::WriteOptions { git: false })
        .unwrap();
    let walked = walk_fixture(&original_root, &ws.fixture).unwrap();
    let queries = QuerySet::builtin().unwrap();
    let grep = GrepRetriever::new(&walked.corpus);
    let bm25 = Bm25Retriever::new(&walked.corpus).unwrap();
    let retrievers: [&dyn Retriever; 3] = [&grep, &bm25, &hybrid];
    let report = run(&walked.corpus, &queries, &retrievers, 10).unwrap();
    eprintln!("{}", report.to_markdown());
    let hybrid_report = report.retriever("hybrid").unwrap();
    assert!(hybrid_report.overall.recall_at_10.unwrap_or(0.0) > 0.0);

    // A vector-store outage must not turn the evaluation into an apparently
    // successful lexical-only run. This database belongs only to this test.
    let mut conn = complete_db.store.acquire().await.unwrap();
    sqlx::query("ALTER TABLE embedding RENAME TO unavailable_embedding")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let error = hybrid
        .search("how do we cancel a subscription", 10)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("every requested search component"),
        "{error}"
    );
}
