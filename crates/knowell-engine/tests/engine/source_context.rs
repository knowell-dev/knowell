use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::Path;
use std::sync::Arc;

use knowell_core::{LineRange, RepoPath};
use knowell_embed::{AnyEmbedder, FakeEmbedder};
use knowell_engine::{Engine, EngineSettings, ParseProductCacheSettings};
use knowell_index::{Priority, SyncOutcome};
use knowell_mcp::tools::{
    BuildContextInput, ContextSection, ContextSelectionStrategy, EntryKind, FetchInput,
    OpenWorkspaceInput, SearchInput, SearchKind,
};
use knowell_mcp::{FileLocator, GapReason, KnowellTools, Target};

use crate::common::{
    DIMS, TestDb, Workspace, access, alice_caller, engine_config, fixture_workspace, git_available,
    indexer_config, name, require_db,
};

async fn source_reader(db: &TestDb, ws: &Workspace, data: &Path, max_lines: u32) -> Engine {
    let embedder = Arc::new(AnyEmbedder::Fake(FakeEmbedder::new(DIMS).unwrap()));
    Engine::builder(db.store.clone(), indexer_config(data))
        .engine_config(&engine_config())
        .embedder(name("local"), Arc::clone(&embedder))
        .embedder(name("cloud"), embedder)
        .workspace(ws.resolved.clone())
        .settings(EngineSettings {
            max_fetch_lines: max_lines,
            ..EngineSettings::default()
        })
        .access(Arc::new(access()))
        .build()
        .await
        .unwrap()
}

/// A fake-provider indexed engine with a caller-selected source line cap.
pub(super) async fn source_engine(
    db: &TestDb,
    ws: &Workspace,
    data: &Path,
    max_lines: u32,
) -> Engine {
    let engine = source_reader(db, ws, data, max_lines).await;
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
    engine
}

fn actual_lines(source: &str, range: LineRange) -> String {
    source
        .split_inclusive('\n')
        .skip(usize::try_from(range.start() - 1).unwrap())
        .take(usize::try_from(range.line_count()).unwrap())
        .collect()
}

fn long_guard() -> String {
    let mut text = "pub fn source_limit_probe(input_bytes: usize, limit: usize) -> Result<usize, &'static str> {\n".to_owned();
    for number in 0..120 {
        text.push_str(&format!("    let _padding_{number} = {number};\n"));
    }
    text.push_str(
        "    if input_bytes > limit { return Err(\"quotaRejected\"); } // oversizeGuard\n",
    );
    text.push_str("    Ok(input_bytes)\n}\n");
    text
}

fn long_assertion() -> String {
    let mut text = "#[test]\nfn verifies_source_limit() {\n".to_owned();
    for number in 0..120 {
        text.push_str(&format!("    let _padding_{number} = {number};\n"));
    }
    text.push_str(
        "    assert_eq!(source_limit_probe(2, 1), Err(\"quotaRejected\")); // assertOversize\n}\n",
    );
    text
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn default_source_context_contains_the_late_guard_and_actual_test_assertion() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let root = ws.project_dir("billing-api");
    std::fs::create_dir_all(root.join("tests")).unwrap();
    let implementation = long_guard();
    let assertions = long_assertion();
    std::fs::write(root.join("src/source-probe.rs"), &implementation).unwrap();
    std::fs::write(root.join("tests/source-probe.rs"), &assertions).unwrap();
    let commit = ws.commit_all("billing-api", "add synthetic late guard and assertion");
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 24).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let context = engine
        .build_context(
            &caller,
            BuildContextInput {
                target: Target::context(opened.context_id),
                task: Some("quotaRejected oversizeGuard assertOversize".to_owned()),
                include: vec![ContextSection::Code, ContextSection::Tests],
                token_budget: Some(3000),
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();

    assert_eq!(
        context.selection.as_ref().unwrap().strategy,
        ContextSelectionStrategy::Source
    );
    assert!(
        context
            .entries
            .iter()
            .any(|entry| entry.kind == EntryKind::Code
                && entry.content.text().contains("if input_bytes > limit"))
    );
    assert!(context.entries.iter().any(|entry| {
        entry.kind == EntryKind::Test
            && entry
                .content
                .text()
                .contains("assert_eq!(source_limit_probe(2, 1)")
    }));
    assert!(context.budget.used <= context.budget.requested);
    for entry in &context.entries {
        assert!(!matches!(
            entry.kind,
            EntryKind::Signature | EntryKind::Skeleton
        ));
        let evidence = entry.evidence.as_ref().unwrap();
        let range = entry.content_lines.unwrap();
        assert_eq!(evidence.commit.as_str(), commit);
        assert_eq!(evidence.lines, range);
        let original = std::fs::read_to_string(root.join(evidence.path.as_str())).unwrap();
        assert_eq!(entry.content.text(), actual_lines(&original, range));
        assert!(range.line_count() <= 24);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_context_keeps_exact_utf8_ranges_inside_a_small_budget() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let root = ws.project_dir("billing-api");
    for (symbol, marker) in [("alpha", "şğö"), ("beta", "é漢字"), ("gamma", "λ🙂")] {
        let text = format!(
            "pub fn budget_probe_{symbol}() -> &'static str {{\r\n    \"shared_source_marker {marker}\"\r\n}}\r\n"
        );
        std::fs::write(root.join(format!("src/source-budget-{symbol}.rs")), text).unwrap();
    }
    let commit = ws.commit_all("billing-api", "add synthetic unicode budget sources");
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 24).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let context = engine
        .build_context(
            &caller,
            BuildContextInput {
                target: Target::context(opened.context_id),
                task: Some("shared_source_marker budget_probe".to_owned()),
                include: vec![ContextSection::Code],
                token_budget: Some(768),
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();

    assert!(!context.entries.is_empty());
    assert_eq!(context.budget.requested, 768);
    assert!(context.budget.used <= 768);
    for entry in &context.entries {
        let evidence = entry.evidence.as_ref().unwrap();
        assert_eq!(evidence.project, name("billing-api"));
        assert_eq!(evidence.commit.as_str(), commit);
        assert!(!matches!(
            entry.kind,
            EntryKind::Signature | EntryKind::Skeleton
        ));
        let original = std::fs::read_to_string(root.join(evidence.path.as_str())).unwrap();
        assert_eq!(
            entry.content.text(),
            actual_lines(&original, entry.content_lines.unwrap())
        );
    }
    assert!(context.entries.iter().any(
        |entry| entry.content.text().contains("shared_source_marker")
            && entry.content.text().contains("\r\n")
    ));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn source_search_excerpt_has_an_exact_fetch_handle_and_optional_body() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let original = long_guard();
    let path = "src/source-probe.rs";
    std::fs::write(ws.project_dir("billing-api").join(path), &original).unwrap();
    let commit = ws.commit_all("billing-api", "add synthetic exact source excerpt");
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 24).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    let mut input = SearchInput {
        target: target.clone(),
        query: "quotaRejected oversizeGuard".to_owned(),
        projects: vec![name("billing-api")],
        path_prefixes: vec![path.to_owned()],
        kinds: vec![SearchKind::Code],
        ..SearchInput::default()
    };
    let searched = engine.search(&caller, input.clone()).await.unwrap();
    let hit = searched
        .hits
        .iter()
        .find(|hit| {
            hit.snippet
                .as_ref()
                .is_some_and(|body| body.text().contains("if input_bytes > limit"))
        })
        .unwrap();
    let range = hit.snippet_lines.unwrap();
    assert!(range.start() > 40);
    assert_eq!(hit.evidence.commit.as_str(), commit);
    assert_eq!(
        hit.snippet.as_ref().unwrap().text(),
        actual_lines(&original, range)
    );
    let displayed_id = hit.snippet_id.clone().unwrap();
    let fetched = engine
        .fetch(
            &caller,
            FetchInput {
                target: target.clone(),
                ids: vec![displayed_id],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let exact = fetched.items.first().unwrap();
    assert_eq!(exact.evidence.lines, range);
    assert_eq!(exact.evidence.commit, hit.evidence.commit);
    assert_eq!(exact.evidence.content_hash, hit.evidence.content_hash);
    assert_eq!(exact.content.text(), hit.snippet.as_ref().unwrap().text());
    assert!(hit.snippet_truncated);
    assert!(!hit.continuation_ids.is_empty());
    let mut lines = BTreeMap::new();
    for (offset, line) in hit
        .snippet
        .as_ref()
        .unwrap()
        .text()
        .split_inclusive('\n')
        .enumerate()
    {
        lines.insert(
            range.start() + u32::try_from(offset).unwrap(),
            line.to_owned(),
        );
    }
    let mut pending: VecDeque<_> = hit.continuation_ids.iter().cloned().collect();
    let mut visited = BTreeSet::new();
    while let Some(id) = pending.pop_front() {
        assert!(
            visited.insert(id.clone()),
            "continuation repeated the same source id"
        );
        assert!(visited.len() < 100);
        let continuation = engine
            .fetch(
                &caller,
                FetchInput {
                    target: target.clone(),
                    ids: vec![id],
                    ..FetchInput::default()
                },
            )
            .await
            .unwrap();
        assert!(continuation.gaps.is_empty());
        let page = continuation.items.first().unwrap();
        assert_eq!(page.evidence.commit.as_str(), commit);
        assert_eq!(page.evidence.content_hash, hit.evidence.content_hash);
        for (offset, line) in page.content.text().split_inclusive('\n').enumerate() {
            let number = page.evidence.lines.start() + u32::try_from(offset).unwrap();
            assert!(
                lines.insert(number, line.to_owned()).is_none(),
                "source pages overlap"
            );
        }
        pending.extend(page.continuation_ids.iter().cloned());
    }
    assert_eq!(lines.into_values().collect::<String>(), original);

    input.include_snippets = Some(false);
    let without_body = engine.search(&caller, input).await.unwrap();
    assert!(without_body.hits.iter().all(|hit| hit.snippet.is_none()
        && hit.snippet_lines.is_none()
        && hit.snippet_id.is_none()));
    assert!(
        without_body
            .hits
            .iter()
            .any(|other| other.id == hit.id && other.evidence == hit.evidence)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn locator_search_skips_source_parsing_and_keeps_semantic_retrieval() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let path = "src/locator-probe.rs";
    let original = long_guard();
    std::fs::write(ws.project_dir("billing-api").join(path), &original).unwrap();
    let commit = ws.commit_all("billing-api", "add synthetic locator source");
    let data = tempfile::tempdir().unwrap();
    let indexed = source_engine(&db, &ws, data.path(), 24).await;
    drop(indexed);

    // This independent observable detects accidental selected-body parsing,
    // even if the resulting snippet is later removed from the response.
    let products = data.path().join("locator-parse-products");
    let embedder = Arc::new(AnyEmbedder::Fake(FakeEmbedder::new(DIMS).unwrap()));
    let engine = Engine::builder(db.store.clone(), indexer_config(data.path()))
        .engine_config(&engine_config())
        .embedder(name("local"), Arc::clone(&embedder))
        .embedder(name("cloud"), embedder)
        .workspace(ws.resolved.clone())
        .settings(EngineSettings {
            max_fetch_lines: 24,
            parse_product_cache: Some(ParseProductCacheSettings::new(&products)),
            ..EngineSettings::default()
        })
        .access(Arc::new(access()))
        .build()
        .await
        .unwrap();
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    assert!(!products.exists());
    let mut input = SearchInput {
        target: Target::context(opened.context_id),
        query: "quotaRejected oversizeGuard".into(),
        projects: vec![name("billing-api")],
        path_prefixes: vec![path.into()],
        languages: vec!["rust".into()],
        kinds: vec![SearchKind::Code],
        include_snippets: Some(false),
        include_diagnostics: Some(true),
        ..SearchInput::default()
    };
    for _ in 0..2 {
        let locators = engine.search(&caller, input.clone()).await.unwrap();
        assert!(!locators.hits.is_empty());
        assert!(
            locators
                .hits
                .iter()
                .all(|hit| hit.evidence.path.as_str() == path
                    && hit.evidence.commit.as_str() == commit
                    && hit.snippet.is_none()
                    && hit.snippet_lines.is_none()
                    && hit.snippet_id.is_none()
                    && hit.continuation_ids.is_empty())
        );
        let work = locators.diagnostics.unwrap();
        assert!(work.locator_only);
        assert_eq!(work.prepared_file_occurrences, 1);
        assert_eq!(work.source_hydrated_paths, 0);
        assert!(work.source_hydration_skipped_paths > 0);
        assert_eq!(work.snippet_read_ms, 0);
        assert!(work.embedding_calls > 0);
        assert!(!work.exact_path_embedding_bypassed);
        assert!(
            !products.exists(),
            "locator retrieval acquired and parsed display bodies"
        );
    }
    input.include_snippets = Some(true);
    let source = engine.search(&caller, input).await.unwrap();
    assert!(source.hits.iter().any(|hit| hit.snippet.is_some()));
    let work = source.diagnostics.unwrap();
    assert!(!work.locator_only);
    assert!(work.source_hydrated_paths > 0);
    assert_eq!(work.source_hydration_skipped_paths, 0);
    assert!(
        products.exists(),
        "source acquisition did not exercise the parse cache control"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn narrow_metadata_catalogs_do_not_replace_unrestricted_or_other_scope_catalogs() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    for (path, body) in [
        ("src/narrow-a.rs", "pub fn narrow_probe_a() {}\n"),
        ("src/narrow-b.rs", "pub fn narrow_probe_b() {}\n"),
    ] {
        std::fs::write(ws.project_dir("billing-api").join(path), body).unwrap();
    }
    ws.commit_all("billing-api", "add synthetic isolated metadata scopes");
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 24).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let mut input = SearchInput {
        target: Target::context(opened.context_id),
        query: "narrow_probe".into(),
        projects: vec![name("billing-api")],
        kinds: vec![SearchKind::Code],
        include_snippets: Some(false),
        include_diagnostics: Some(true),
        ..SearchInput::default()
    };
    for path in ["src/narrow-a.rs", "src/narrow-b.rs", "src/narrow-a.rs"] {
        input.path_prefixes = vec![format!(" {path} ")];
        let scoped = engine.search(&caller, input.clone()).await.unwrap();
        assert!(!scoped.hits.is_empty());
        assert!(
            scoped
                .hits
                .iter()
                .all(|hit| hit.evidence.path.as_str() == path)
        );
        assert_eq!(scoped.diagnostics.unwrap().prepared_file_occurrences, 1);
    }
    input.path_prefixes.clear();
    let broad = engine.search(&caller, input.clone()).await.unwrap();
    assert!(broad.diagnostics.unwrap().prepared_file_occurrences > 2);
    for path in ["src/narrow-a.rs", "src/narrow-b.rs"] {
        assert!(
            broad
                .hits
                .iter()
                .any(|hit| hit.evidence.path.as_str() == path)
        );
    }
    input.path_prefixes = vec!["src/narrow-a.rs".into()];
    input.languages = vec!["typescript".into()];
    let wrong_language = engine.search(&caller, input.clone()).await.unwrap();
    assert!(wrong_language.hits.is_empty());
    assert_eq!(
        wrong_language
            .diagnostics
            .unwrap()
            .prepared_file_occurrences,
        0
    );
    input.languages = vec!["rust".into()];
    let rust = engine.search(&caller, input).await.unwrap();
    assert!(!rust.hits.is_empty());
    assert_eq!(rust.diagnostics.unwrap().prepared_file_occurrences, 1);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unavailable_source_bodies_are_acquisition_omissions_not_empty_retrieval() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let path = "src/source-missing-body.rs";
    let original = "pub fn acquisition_omission_probe() { unavailable_body_marker(); }\n";
    std::fs::write(ws.project_dir("billing-api").join(path), original).unwrap();
    ws.commit_all("billing-api", "add synthetic acquisition omission source");
    let data = tempfile::tempdir().unwrap();
    let indexed = source_engine(&db, &ws, data.path(), 24).await;
    drop(indexed);
    let mut conn = db.store.acquire().await.unwrap();
    let hash: Vec<u8> = sqlx::query_scalar(
        "SELECT content_hash FROM file_version WHERE path = $1 ORDER BY valid_from DESC LIMIT 1",
    )
    .bind(path)
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE content SET redacted_text = NULL, redacted_line_count = NULL WHERE hash = $1",
    )
    .bind(hash)
    .execute(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let engine = source_reader(&db, &ws, data.path(), 24).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    let searched = engine
        .search(
            &caller,
            SearchInput {
                target: target.clone(),
                query: "acquisition_omission_probe unavailable_body_marker".into(),
                path_prefixes: vec![path.into()],
                kinds: vec![SearchKind::Code],
                ..SearchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(searched.hits.is_empty());
    assert!(
        searched
            .gaps
            .iter()
            .any(|gap| gap.message.contains("source acquisition omission"))
    );
    assert!(
        !searched
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::NoMatches)
    );
    let context = engine
        .build_context(
            &caller,
            BuildContextInput {
                target,
                task: Some("acquisition_omission_probe unavailable_body_marker".into()),
                include: vec![ContextSection::Code],
                token_budget: Some(3000),
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        context
            .gaps
            .iter()
            .any(|gap| gap.message.contains("source acquisition omission"))
    );
    assert!(
        !context
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::NoMatches)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn fetch_continuation_recovers_remaining_pinned_source_bytes() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let original = "pub fn continuation_probe() {\r\n    consume();\r\n}\r\n";
    let path = RepoPath::new("src/source-continuation.rs").unwrap();
    std::fs::write(ws.project_dir("billing-api").join(path.as_str()), original).unwrap();
    let commit = ws.commit_all("billing-api", "add synthetic source continuation");
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 2).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    let first = engine
        .fetch(
            &caller,
            FetchInput {
                target: target.clone(),
                paths: vec![FileLocator {
                    project: name("billing-api"),
                    path,
                    lines: None,
                }],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let first = first.items.first().unwrap();
    assert!(first.truncated);
    assert_eq!(first.evidence.lines, LineRange::new(1, 2).unwrap());
    assert_eq!(first.continuation_ids.len(), 1);
    let remaining = engine
        .fetch(
            &caller,
            FetchInput {
                target,
                ids: first.continuation_ids.clone(),
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let remaining = remaining.items.first().unwrap();
    assert!(!remaining.truncated);
    assert!(remaining.continuation_ids.is_empty());
    assert_eq!(remaining.evidence.lines, LineRange::new(3, 3).unwrap());
    assert_eq!(remaining.evidence.commit.as_str(), commit);
    assert_eq!(remaining.evidence.content_hash, first.evidence.content_hash);
    assert_eq!(
        format!("{}{}", first.content.text(), remaining.content.text()),
        original
    );
}
