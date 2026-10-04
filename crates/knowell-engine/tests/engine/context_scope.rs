use knowell_core::{LineRange, RepoPath};
use knowell_mcp::tools::{
    BuildContextInput, ContextSection, ContextSelectionStrategy, EntryKind, FetchInput,
    OpenWorkspaceInput,
};
use knowell_mcp::{FileLocator, GapReason, KnowellTools, Target, ViewLayer};

use crate::common::{Workspace, alice_caller, fixture_workspace, git_available, name, require_db};
use crate::source_context::source_engine;

const READER: &str = "pub fn read_format(limit: usize) -> Result<usize, &'static str> {\n    if limit > 12 { return Err(\"synthetic_native_marker\"); }\n    Ok(limit)\n}\n";
const MAPPER: &str = "pub struct NativeDocument { pub limit: usize }\npub fn to_document(limit: usize) -> NativeDocument {\n    NativeDocument { limit }\n}\n";
const ASSERTION: &str = "#[test]\nfn verifies_native_route() {\n    assert!(read_format(20).is_err()); // synthetic_native_marker\n}\n";

fn navigation_workspace() -> Workspace {
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| matches!(project.name.as_str(), "billing-api" | "storefront-web"));
    for project in ["billing-api", "storefront-web"] {
        let root = ws.project_dir(project);
        std::fs::create_dir_all(root.join("src/native")).unwrap();
        std::fs::create_dir_all(root.join("src/other")).unwrap();
        for (path, source) in [
            ("src/native/reader.rs", READER),
            ("src/native/mapping.rs", MAPPER),
            ("src/native/reader_test.rs", ASSERTION),
            (
                "src/native/reader.hpp",
                "// synthetic_native_marker\nint read_format(int limit) { return limit; }\n",
            ),
            (
                "src/native/guide.md",
                "# synthetic_native_marker\nread_format to_document verifies_native_route overview\n",
            ),
            (
                "src/other/mapping.rs",
                "pub fn to_document(limit: usize) -> usize { limit + 99 }\npub fn other_format_probe() -> usize { 41 }\n",
            ),
        ] {
            std::fs::write(root.join(path), source).unwrap();
        }
        ws.commit_all(
            project,
            "add synthetic format navigation and name collisions",
        );
    }
    ws
}

fn source_lines(source: &str, range: LineRange) -> String {
    source
        .split_inclusive('\n')
        .skip(usize::try_from(range.start() - 1).unwrap())
        .take(usize::try_from(range.line_count()).unwrap())
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn hard_context_filters_admit_only_the_selected_project_path_and_language_for_all_selectors()
{
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = navigation_workspace();
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 24).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    for strategy in [
        ContextSelectionStrategy::Source,
        ContextSelectionStrategy::Rank,
        ContextSelectionStrategy::Mmr,
        ContextSelectionStrategy::RoleCoverage,
        ContextSelectionStrategy::BoundedBundles,
    ] {
        let pack = engine
            .build_context(
                &caller,
                BuildContextInput {
                    target: target.clone(),
                    task: Some(
                        "synthetic_native_marker read_format to_document verifies_native_route"
                            .to_owned(),
                    ),
                    projects: vec![name("billing-api")],
                    path_prefixes: vec!["src/native/".to_owned()],
                    languages: vec!["rust".to_owned()],
                    focus_symbols: vec!["read_format".to_owned()],
                    include: vec![ContextSection::Code, ContextSection::Tests],
                    token_budget: Some(2000),
                    selection_strategy: Some(strategy),
                    ..BuildContextInput::default()
                },
            )
            .await
            .unwrap();
        assert!(!pack.entries.is_empty(), "{strategy:?}: {pack:?}");
        assert!(pack.budget.used <= pack.budget.requested);
        assert!(
            pack.entries.iter().all(|entry| {
                entry.evidence.as_ref().is_some_and(|evidence| {
                    evidence.project.as_str() == "billing-api"
                        && evidence.path.as_str().starts_with("src/native/")
                        && evidence.path.as_str().ends_with(".rs")
                })
            }),
            "{strategy:?}: {pack:?}"
        );
        assert!(
            pack.entries.iter().any(|entry| {
                entry
                    .evidence
                    .as_ref()
                    .is_some_and(|evidence| evidence.path.as_str() == "src/native/mapping.rs")
            }),
            "{strategy:?}: {pack:?}"
        );
        assert!(
            pack.entries
                .iter()
                .any(|entry| entry.section == ContextSection::Tests),
            "{strategy:?}: {pack:?}"
        );
        if strategy == ContextSelectionStrategy::Source {
            assert!(
                pack.entries.iter().any(|entry| {
                    entry.kind == EntryKind::Test
                        && entry
                            .content
                            .text()
                            .contains("assert!(read_format(20).is_err())")
                }),
                "{strategy:?}: {pack:?}"
            );
        }
        for entry in pack.entries.iter().filter(|entry| {
            strategy == ContextSelectionStrategy::Source
                || entry.why_relevant == "verified definition of a named focus symbol"
        }) {
            let evidence = entry.evidence.as_ref().unwrap();
            let original =
                std::fs::read_to_string(ws.project_dir("billing-api").join(evidence.path.as_str()))
                    .unwrap();
            assert_eq!(
                entry.content.text(),
                source_lines(&original, evidence.lines)
            );
            assert_eq!(entry.content_lines, Some(evidence.lines));
        }
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn soft_focus_paths_keep_other_task_sources_and_short_symbol_collisions_remain_visible() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = navigation_workspace();
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 24).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    let soft = engine
        .build_context(
            &caller,
            BuildContextInput {
                target: target.clone(),
                task: Some("other_format_probe".to_owned()),
                projects: vec![name("billing-api")],
                languages: vec!["rust".to_owned()],
                focus_paths: vec![FileLocator {
                    project: name("billing-api"),
                    path: RepoPath::new("src/native/reader.rs").unwrap(),
                    lines: None,
                }],
                include: vec![ContextSection::Code],
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        soft.entries
            .iter()
            .any(|entry| entry.content.text() == READER)
    );
    assert!(
        soft.entries
            .iter()
            .any(|entry| entry.content.text().contains("other_format_probe"))
    );

    let mut input = BuildContextInput {
        target,
        task: Some("synthetic_native_marker".to_owned()),
        projects: vec![name("billing-api")],
        languages: vec!["rust".to_owned()],
        focus_symbols: vec!["to_document".to_owned()],
        include: vec![ContextSection::Code],
        ..BuildContextInput::default()
    };
    let ambiguous = engine.build_context(&caller, input.clone()).await.unwrap();
    assert!(
        ambiguous
            .uncertainties
            .iter()
            .any(|notice| notice.contains("matches multiple definitions"))
    );
    assert!(
        ambiguous
            .entries
            .iter()
            .all(|entry| { entry.why_relevant != "verified definition of a named focus symbol" })
    );

    input.focus_symbols = vec!["src/native/mapping.rs#to_document".to_owned()];
    let exact = engine.build_context(&caller, input.clone()).await.unwrap();
    let anchor = exact
        .entries
        .iter()
        .find(|entry| entry.why_relevant == "verified definition of a named focus symbol")
        .unwrap();
    assert_eq!(
        anchor.evidence.as_ref().unwrap().path.as_str(),
        "src/native/mapping.rs"
    );
    assert!(anchor.content.text().contains("NativeDocument { limit }"));

    input.focus_symbols = vec!["WrongReader.to_document".to_owned()];
    let unresolved = engine.build_context(&caller, input).await.unwrap();
    assert!(
        unresolved
            .uncertainties
            .iter()
            .any(|notice| notice.contains("no verified definition"))
    );
    assert!(
        unresolved
            .entries
            .iter()
            .all(|entry| { entry.why_relevant != "verified definition of a named focus symbol" })
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn named_focus_sources_obey_sections_and_language_filters_before_body_acquisition() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = navigation_workspace();
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 24).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    for (path, languages) in [
        ("src/native/guide.md", vec!["rust".to_owned()]),
        ("src/native/guide.md", vec![]),
        ("src/native/reader.hpp", vec!["rust".to_owned()]),
    ] {
        let context = engine
            .build_context(
                &caller,
                BuildContextInput {
                    target: Target::context(opened.context_id.clone()),
                    task: Some("synthetic_native_marker".to_owned()),
                    projects: vec![name("billing-api")],
                    path_prefixes: vec!["src/native/".to_owned()],
                    languages,
                    focus_paths: vec![FileLocator {
                        project: name("billing-api"),
                        path: RepoPath::new(path).unwrap(),
                        lines: None,
                    }],
                    include: vec![ContextSection::Code],
                    ..BuildContextInput::default()
                },
            )
            .await
            .unwrap();
        assert!(
            context
                .gaps
                .iter()
                .any(|gap| gap.reason == GapReason::FiltersExcludedAll)
        );
        assert!(context.entries.iter().all(|entry| {
            entry
                .evidence
                .as_ref()
                .is_none_or(|evidence| evidence.path.as_str() != path)
        }));
    }
    let protected = engine
        .build_context(
            &caller,
            BuildContextInput {
                target: Target::context(opened.context_id),
                task: Some("synthetic_native_marker".to_owned()),
                projects: vec![name("billing-api")],
                path_prefixes: vec!["src/native/".to_owned()],
                languages: vec!["rust".to_owned()],
                focus_paths: vec![FileLocator {
                    project: name("billing-api"),
                    path: RepoPath::new("src/native/.env.synthetic").unwrap(),
                    lines: None,
                }],
                include: vec![ContextSection::Code],
                ..BuildContextInput::default()
            },
        )
        .await
        .unwrap();
    assert!(
        protected
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::ExcludedByPolicy)
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn focused_sources_use_the_pinned_personal_version_and_keep_fetch_identity() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = navigation_workspace();
    let data = tempfile::tempdir().unwrap();
    let engine = source_engine(&db, &ws, data.path(), 24).await;
    let worktree = ws.dir.path().join("native-personal");
    ws.git(
        "billing-api",
        &[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/native-personal",
            worktree.to_str().unwrap(),
        ],
    );
    let changed = READER.replace("limit > 12", "limit > 7");
    std::fs::write(worktree.join("src/native/reader.rs"), &changed).unwrap();
    std::fs::write(
        worktree.join("src/other/mapping.rs"),
        "struct First; struct Second; impl First { fn to_document() {} } impl Second { fn to_document() {} }\n",
    )
    .unwrap();
    let caller = alice_caller();
    let opened = engine
        .open_workspace(
            &caller,
            OpenWorkspaceInput {
                working_directory: Some(worktree.to_string_lossy().into_owned()),
                ..OpenWorkspaceInput::default()
            },
        )
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    for focus_symbol in [false, true] {
        let context = engine
            .build_context(
                &caller,
                BuildContextInput {
                    target: target.clone(),
                    task: Some("read_format".to_owned()),
                    projects: vec![name("billing-api")],
                    path_prefixes: vec!["src/native/reader.rs".to_owned()],
                    languages: vec!["rust".to_owned()],
                    focus_symbols: if focus_symbol {
                        vec!["read_format".to_owned()]
                    } else {
                        vec![]
                    },
                    focus_paths: if focus_symbol {
                        vec![]
                    } else {
                        vec![FileLocator {
                            project: name("billing-api"),
                            path: RepoPath::new("src/native/reader.rs").unwrap(),
                            lines: None,
                        }]
                    },
                    include: vec![ContextSection::Code],
                    ..BuildContextInput::default()
                },
            )
            .await
            .unwrap();
        let source = context
            .entries
            .iter()
            .find(|entry| entry.content.text().contains("limit > 7"))
            .unwrap();
        assert_eq!(source.evidence.as_ref().unwrap().layer, ViewLayer::Personal);
        assert!(!source.content.text().contains("limit > 12"));
        let fetched = engine
            .fetch(
                &caller,
                FetchInput {
                    target: target.clone(),
                    ids: vec![source.id.clone()],
                    ..FetchInput::default()
                },
            )
            .await
            .unwrap();
        assert_eq!(
            fetched.items.first().unwrap().content.text(),
            source.content.text()
        );
        assert_eq!(
            fetched.items.first().unwrap().evidence.content_hash,
            source.evidence.as_ref().unwrap().content_hash
        );
    }

    let input = BuildContextInput {
        target,
        task: Some("to_document".to_owned()),
        projects: vec![name("billing-api")],
        path_prefixes: vec!["src/other/mapping.rs".to_owned()],
        languages: vec!["rust".to_owned()],
        focus_symbols: vec!["to_document".to_owned()],
        include: vec![ContextSection::Code],
        ..BuildContextInput::default()
    };
    let ambiguous = engine.build_context(&caller, input.clone()).await.unwrap();
    assert!(
        ambiguous
            .uncertainties
            .iter()
            .any(|notice| notice.contains("matches multiple definitions")),
        "{:?}",
        ambiguous.uncertainties
    );
    assert!(
        ambiguous
            .entries
            .iter()
            .all(|entry| { entry.why_relevant != "verified definition of a named focus symbol" })
    );

    let exact = engine
        .build_context(
            &caller,
            BuildContextInput {
                focus_symbols: vec!["Second.to_document".to_owned()],
                ..input
            },
        )
        .await
        .unwrap();
    let anchor = exact
        .entries
        .iter()
        .find(|entry| entry.why_relevant == "verified definition of a named focus symbol")
        .unwrap();
    let evidence = anchor.evidence.as_ref().unwrap();
    assert_eq!(evidence.layer, ViewLayer::Personal);
    assert_eq!(evidence.symbol.as_deref(), Some("Second.to_document"));
    assert!(
        anchor
            .content
            .text()
            .contains("impl Second { fn to_document() {} }")
    );
}
