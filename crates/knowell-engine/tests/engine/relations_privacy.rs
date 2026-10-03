//! Relation queries keep source reads inside the pinned project's policy.

use std::collections::BTreeSet;
use std::sync::Arc;

use knowell_config::{Origin, Sourced};
use knowell_core::{RepoPath, TrackTarget};
use knowell_engine::Engine;
use knowell_index::Priority;
use knowell_mcp::tools::{
    AnalyzeImpactInput, ChangeKind, ChangeSubject, FlowDirection, OpenWorkspaceInput,
    ResumeTaskInput, RiskCode, SaveCheckpointInput, TraceFlowInput,
};
use knowell_mcp::{GapReason, IndexState, KnowellTools, SymbolRef, Target, ToolError};

use crate::common::{
    TestDb, Workspace, access, alice_caller, bob_caller, fixture_workspace, git_available,
    indexer_config, name, require_db,
};

const PROJECT: &str = "billing-api";
const ROOT: &str = "packages/app";
const SUBJECT: &str = "src/scope.ts";
const SCOPE_TEXT: &str = "export function ScopeProbe(value: number): number {\n  const first = value + 1;\n  const second = first * 2;\n  return second;\n}\n";
const RENAME_TEXT: &str = "export function RenameProbe(value: number): number {\n  const first = value + 1;\n  const second = first * 2;\n  const third = second + 3;\n  const fourth = third * 4;\n  return fourth;\n}\n";

fn relative(path: &str) -> RepoPath {
    RepoPath::new(path).unwrap()
}

fn commit_target(commit: &str) -> TrackTarget {
    format!("commit:{commit}").parse().unwrap()
}

fn write(ws: &Workspace, path: &str, text: &str) {
    let path = ws.project_dir(PROJECT).join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
}

fn workspace() -> Workspace {
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == PROJECT);
    let project = &mut ws.resolved.projects[0];
    project.root = Some(relative(ROOT));
    project.embedding.provider = None;
    project.embedding.model = None;
    project.exclude.push(Sourced {
        value: "src/excluded-*.ts".to_owned(),
        origin: Origin::Project,
    });
    write(&ws, &format!("{ROOT}/{SUBJECT}"), SCOPE_TEXT);
    write(&ws, &format!("{ROOT}/src/original.ts"), RENAME_TEXT);
    for client in ["a", "b", "c"] {
        write(
            &ws,
            &format!("{ROOT}/src/client-{client}.ts"),
            &format!(
                "import {{ ScopeProbe }} from './scope';\nexport function client_{client}() {{ return ScopeProbe(3); }}\n",
            ),
        );
    }
    for path in [
        "outside/old.ts",
        "packages/application/old.ts",
        "packages/app/src/excluded-old.ts",
        "packages/app/quarantine/old.ts",
    ] {
        write(&ws, path, &format!("// synthetic {path}\n{RENAME_TEXT}"));
    }
    write(
        &ws,
        "packages/app/.env",
        "KNOWELL_CANARY_RELATIONS_EXCLUDED=synthetic-only\n",
    );
    ws.commit_all(PROJECT, "add synthetic scoped relation fixtures");
    ws
}

async fn engine(db: &TestDb, ws: &Workspace, data: &std::path::Path) -> Engine {
    let mut config = indexer_config(data);
    config.content.excluded_dirs.push("quarantine".to_owned());
    config.limits.max_file_bytes = 1024;
    Engine::builder(db.store.clone(), config)
        .workspace(ws.resolved.clone())
        .access(Arc::new(access()))
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
            .all(|outcome| !matches!(outcome, knowell_index::SyncOutcome::Failed { .. })),
        "{outcomes:?}"
    );
}

fn trace(target: Target, limit: u32) -> TraceFlowInput {
    TraceFlowInput {
        target,
        symbol: Some("ScopeProbe".to_owned()),
        project: Some(name(PROJECT)),
        direction: Some(FlowDirection::Upstream),
        limit: Some(limit),
        ..TraceFlowInput::default()
    }
}

fn diff(target: Target, base: &str, head: Option<&str>) -> AnalyzeImpactInput {
    AnalyzeImpactInput {
        target,
        change: Some(ChangeSubject::Diff {
            project: name(PROJECT),
            base: commit_target(base),
            head: head.map(commit_target),
        }),
        ..AnalyzeImpactInput::default()
    }
}

/// The committed tree still names the synthetic object; only a content read
/// can notice its absence. This distinguishes filtering before reads from
/// filtering a completed whole-repository diff.
fn hide_blob(ws: &Workspace, commit: &str, path: &str, label: &str) {
    let object = ws.git(PROJECT, &["rev-parse", &format!("{commit}:{path}")]);
    assert_eq!(object.len(), 40);
    assert!(object.chars().all(|c| c.is_ascii_hexdigit()));
    let source = ws
        .project_dir(PROJECT)
        .join(".git/objects")
        .join(&object[..2])
        .join(&object[2..]);
    std::fs::rename(source, ws.dir.path().join(format!("hidden-{label}.blob"))).unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn impact_and_resume_filter_before_reads_and_preserve_edited_renames() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace();
    let base = ws.git(PROJECT, &["rev-parse", "HEAD"]);
    let data = tempfile::tempdir().unwrap();
    let engine = engine(&db, &ws, data.path()).await;
    index(&engine, &ws).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let old_target = Target::context(opened.context_id.clone());
    let saved = engine
        .save_checkpoint(
            &caller,
            SaveCheckpointInput {
                target: old_target.clone(),
                goal: Some("review the synthetic scoped rename".to_owned()),
                progress: "reviewed the source at the first commit".to_owned(),
                ..SaveCheckpointInput::default()
            },
        )
        .await
        .unwrap();
    write(
        &ws,
        &format!("{ROOT}/{SUBJECT}"),
        &SCOPE_TEXT.replace("value + 1", "value + 2"),
    );
    std::fs::rename(
        ws.project_dir(PROJECT)
            .join(format!("{ROOT}/src/original.ts")),
        ws.project_dir(PROJECT)
            .join(format!("{ROOT}/src/renamed.ts")),
    )
    .unwrap();
    write(
        &ws,
        &format!("{ROOT}/src/renamed.ts"),
        &RENAME_TEXT.replace("third * 4", "third * 5"),
    );
    for (old, new) in [
        ("outside/old.ts", "outside/new.ts"),
        ("packages/application/old.ts", "packages/application/new.ts"),
        (
            "packages/app/src/excluded-old.ts",
            "packages/app/src/excluded-new.ts",
        ),
        (
            "packages/app/quarantine/old.ts",
            "packages/app/quarantine/new.ts",
        ),
    ] {
        std::fs::remove_file(ws.project_dir(PROJECT).join(old)).unwrap();
        write(
            &ws,
            new,
            &format!(
                "// distinct synthetic {new}\n{}",
                RENAME_TEXT.replace("third * 4", "third * 6")
            ),
        );
    }
    let head = ws.commit_all(PROJECT, "edit allowed rename and forbidden candidates");
    for (path, label) in [
        ("outside/new.ts", "outside"),
        ("packages/application/new.ts", "component-boundary"),
        ("packages/app/src/excluded-new.ts", "custom-exclusion"),
        ("packages/app/quarantine/new.ts", "configured-directory"),
    ] {
        hide_blob(&ws, &head, path, label);
    }

    let before = engine.indexer().stats();
    let impact = engine
        .analyze_impact(&caller, diff(Target::default(), &base, Some(&head)))
        .await
        .unwrap();
    assert!(
        impact.changed.iter().any(|item| item.name == "ScopeProbe"),
        "{impact:?}"
    );
    assert!(
        impact.changed.iter().any(|item| item.name == "RenameProbe"),
        "{impact:?}"
    );
    assert!(
        impact
            .changed
            .iter()
            .all(|item| item.evidence.commit.as_str() == base
                && item.evidence.index_state == IndexState::Stale
                && item.evidence.path.as_str().starts_with("src/")),
        "{impact:?}"
    );
    assert_eq!(
        engine.indexer().stats(),
        before,
        "a committed diff must not index source"
    );
    let serialized = serde_json::to_string(&impact).unwrap();
    for forbidden in [
        "outside/",
        "excluded-",
        "quarantine/",
        "application/",
        "KNOWELL_CANARY_RELATIONS_EXCLUDED",
    ] {
        assert!(
            !serialized.contains(forbidden),
            "unexpected scoped output: {forbidden}"
        );
    }

    index(&engine, &ws).await;
    let resumed = engine
        .resume_task(
            &caller,
            ResumeTaskInput {
                target: Target::default(),
                task_id: Some(saved.task_id.clone()),
                ..ResumeTaskInput::default()
            },
        )
        .await
        .unwrap();
    assert!(resumed.gaps.is_empty(), "{resumed:?}");
    let changes = resumed.task.unwrap().changed_since;
    assert_eq!(changes.len(), 2, "{changes:?}");
    assert!(
        changes.iter().any(|change| change.path.as_str() == SUBJECT
            && change.change == ChangeKind::Modified
            && change.previous_path.is_none()),
        "{changes:?}"
    );
    assert!(
        changes
            .iter()
            .any(|change| change.path.as_str() == "src/renamed.ts"
                && change.change == ChangeKind::Renamed
                && change
                    .previous_path
                    .as_ref()
                    .is_some_and(|path| path.as_str() == "src/original.ts")),
        "{changes:?}"
    );
    assert!(
        changes
            .iter()
            .all(|change| change.from_commit.as_str() == base && change.to_commit.as_str() == head)
    );
    let old_trace = engine
        .trace_flow(&caller, trace(old_target, 50))
        .await
        .unwrap();
    assert!(
        old_trace
            .nodes
            .iter()
            .filter_map(|node| node.evidence.as_ref())
            .all(|evidence| evidence.commit.as_str() == base
                && evidence.index_state == IndexState::Current)
    );
    hide_blob(
        &ws,
        &head,
        &format!("{ROOT}/src/renamed.ts"),
        "allowed-resume",
    );
    let failed_resume = engine
        .resume_task(
            &caller,
            ResumeTaskInput {
                task_id: Some(saved.task_id),
                ..ResumeTaskInput::default()
            },
        )
        .await;
    assert!(
        matches!(failed_resume, Err(ToolError::Internal(_))),
        "{failed_resume:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn trace_limit_is_global_across_symbol_and_file_starts() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace();
    let data = tempfile::tempdir().unwrap();
    let engine = engine(&db, &ws, data.path()).await;
    index(&engine, &ws).await;
    let caller = alice_caller();
    let full = engine
        .trace_flow(&caller, trace(Target::default(), 50))
        .await
        .unwrap();
    assert!(full.nodes.len() >= 5 && full.edges.len() >= 3, "{full:?}");
    assert!(!full.truncated);
    assert!(
        !full
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RelationsNotReady)
    );
    for limit in [1, 2, 3] {
        let clipped = engine
            .trace_flow(&caller, trace(Target::default(), limit))
            .await
            .unwrap();
        assert!(
            clipped.nodes.len() <= usize::try_from(limit).unwrap(),
            "{clipped:?}"
        );
        assert_eq!(clipped.nodes.first(), full.nodes.first());
        assert!(
            clipped.truncated
                && clipped
                    .gaps
                    .iter()
                    .any(|gap| gap.reason == GapReason::LimitReached)
        );
        let nodes: BTreeSet<_> = clipped.nodes.iter().map(|node| &node.node).collect();
        assert!(
            clipped
                .edges
                .iter()
                .all(|edge| nodes.contains(&edge.from) && nodes.contains(&edge.to))
        );
        let repeated = engine
            .trace_flow(&caller, trace(Target::default(), limit))
            .await
            .unwrap();
        assert_eq!(clipped, repeated);
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn graph_tools_report_missing_index_and_ref_without_fallback_or_jobs() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace();
    let base = ws.git(PROJECT, &["rev-parse", "HEAD"]);
    let data = tempfile::tempdir().unwrap();
    let engine = engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let stats = engine.indexer().stats();
    let not_indexed = engine
        .trace_flow(&caller, trace(Target::default(), 50))
        .await
        .unwrap();
    assert!(not_indexed.nodes.is_empty() && not_indexed.edges.is_empty());
    assert!(
        not_indexed
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::ProjectNotIndexed
                && gap.project.as_ref() == Some(&name(PROJECT)))
    );
    let subjects = vec![
        ChangeSubject::Symbol {
            symbol: SymbolRef {
                id: None,
                symbol: Some("ScopeProbe".to_owned()),
                project: Some(name(PROJECT)),
            },
        },
        ChangeSubject::File {
            project: name(PROJECT),
            path: relative(SUBJECT),
        },
        ChangeSubject::Diff {
            project: name(PROJECT),
            base: commit_target(&base),
            head: None,
        },
        ChangeSubject::Patch {
            project: name(PROJECT),
            patch: "--- a/src/scope.ts\n+++ b/src/scope.ts\n@@ -1,1 +1,1 @@\n-old\n+new\n"
                .to_owned(),
        },
    ];
    for change in &subjects {
        let impact = engine
            .analyze_impact(
                &caller,
                AnalyzeImpactInput {
                    change: Some(change.clone()),
                    ..AnalyzeImpactInput::default()
                },
            )
            .await
            .unwrap();
        assert!(impact.changed.is_empty() && impact.impacted.is_empty() && impact.risk.is_none());
        assert!(
            impact
                .gaps
                .iter()
                .any(|gap| gap.reason == GapReason::ProjectNotIndexed)
        );
    }
    assert_eq!(engine.indexer().stats(), stats);
    index(&engine, &ws).await;
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let stable = Target::context(opened.context_id);
    let old_trace = engine
        .trace_flow(&caller, trace(stable.clone(), 50))
        .await
        .unwrap();
    assert!(!old_trace.nodes.is_empty());
    ws.git(PROJECT, &["update-ref", "-d", "refs/heads/main"]);
    let stats = engine.indexer().stats();
    let missing = engine
        .trace_flow(&caller, trace(Target::default(), 50))
        .await
        .unwrap();
    assert!(missing.nodes.is_empty() && missing.edges.is_empty());
    assert!(
        missing
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RefNotFound)
    );
    for change in subjects {
        let impact = engine
            .analyze_impact(
                &caller,
                AnalyzeImpactInput {
                    change: Some(change),
                    ..AnalyzeImpactInput::default()
                },
            )
            .await
            .unwrap();
        assert!(impact.changed.is_empty() && impact.impacted.is_empty() && impact.risk.is_none());
        assert!(
            impact
                .gaps
                .iter()
                .any(|gap| gap.reason == GapReason::RefNotFound)
        );
    }
    let reused = engine.trace_flow(&caller, trace(stable, 50)).await.unwrap();
    assert_eq!(
        old_trace, reused,
        "an existing context retains its historical pins"
    );
    assert_eq!(engine.indexer().stats(), stats);
    // This project exists in the configuration but is invisible to Bob. An
    // unavailable source also proves authorization precedes the source probe.
    let mut with_hidden = ws.resolved.clone();
    let mut hidden = with_hidden.projects[0].clone();
    hidden.name = name("storefront-web");
    hidden.path = ws.root().join("missing-hidden-repository");
    with_hidden.projects.push(hidden);
    engine.add_workspace(&with_hidden).await.unwrap();
    let unauthorized = bob_caller();
    for project in ["storefront-web", "unknown-project"] {
        let result = engine
            .trace_flow(
                &unauthorized,
                TraceFlowInput {
                    symbol: Some("ScopeProbe".to_owned()),
                    project: Some(name(project)),
                    ..TraceFlowInput::default()
                },
            )
            .await;
        assert!(matches!(result, Err(ToolError::NotFound(_))), "{result:?}");
    }
    let opened = engine
        .open_workspace(&unauthorized, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let context = Target::context(opened.context_id);
    let stats = engine.indexer().stats();
    let mut conn = db.store.acquire().await.unwrap();
    let jobs: serde_json::Value = sqlx::query_scalar(
        "SELECT coalesce(jsonb_agg(to_jsonb(job) ORDER BY id), '[]'::jsonb) FROM job",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    drop(conn);
    let mut missing = Vec::new();
    for project in ["storefront-web", "unknown-project"] {
        let result = engine
            .trace_flow(
                &unauthorized,
                TraceFlowInput {
                    target: context.clone(),
                    symbol: Some("ScopeProbe".to_owned()),
                    project: Some(name(project)),
                    ..TraceFlowInput::default()
                },
            )
            .await
            .unwrap();
        assert!(result.nodes.is_empty() && result.edges.is_empty());
        assert!(result.job.is_none() && !result.truncated);
        assert!(
            result
                .gaps
                .iter()
                .any(|gap| gap.reason == GapReason::NotFound)
        );
        assert!(result.gaps.iter().any(|gap| {
            gap.reason == GapReason::RefNotFound && gap.project.as_ref() == Some(&name(PROJECT))
        }));
        missing.push(result);
    }
    assert_eq!(
        missing[0], missing[1],
        "cached unknown and invisible starts reveal no data"
    );
    assert_eq!(engine.indexer().stats(), stats);
    let mut conn = db.store.acquire().await.unwrap();
    let after_jobs: serde_json::Value = sqlx::query_scalar(
        "SELECT coalesce(jsonb_agg(to_jsonb(job) ORDER BY id), '[]'::jsonb) FROM job",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap();
    assert_eq!(after_jobs, jobs);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_diff_distinguishes_missing_refs_and_unreadable_allowed_content() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace();
    let base = ws.git(PROJECT, &["rev-parse", "HEAD"]);
    let data = tempfile::tempdir().unwrap();
    let engine = engine(&db, &ws, data.path()).await;
    index(&engine, &ws).await;
    let caller = alice_caller();
    let missing = engine
        .analyze_impact(
            &caller,
            AnalyzeImpactInput {
                change: Some(ChangeSubject::Diff {
                    project: name(PROJECT),
                    base: "branch:missing-base".parse().unwrap(),
                    head: None,
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(missing.changed.is_empty() && missing.risk.is_none());
    assert!(
        missing
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RefNotFound)
    );
    let missing_head = engine
        .analyze_impact(
            &caller,
            AnalyzeImpactInput {
                change: Some(ChangeSubject::Diff {
                    project: name(PROJECT),
                    base: commit_target(&base),
                    head: Some("branch:missing-head".parse().unwrap()),
                }),
                ..AnalyzeImpactInput::default()
            },
        )
        .await
        .unwrap();
    assert!(missing_head.changed.is_empty() && missing_head.risk.is_none());
    assert!(
        missing_head
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::RefNotFound)
    );
    write(
        &ws,
        &format!("{ROOT}/{SUBJECT}"),
        &SCOPE_TEXT.replace("value + 1", "value + 4"),
    );
    let head = ws.commit_all(PROJECT, "edit synthetic allowed object");
    hide_blob(&ws, &head, &format!("{ROOT}/{SUBJECT}"), "allowed-corrupt");
    let result = engine
        .analyze_impact(&caller, diff(Target::default(), &base, Some(&head)))
        .await;
    assert!(matches!(result, Err(ToolError::Internal(_))), "{result:?}");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn committed_diff_reports_configured_size_limit_without_false_symbol_removal() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace();
    let base = ws.git(PROJECT, &["rev-parse", "HEAD"]);
    let data = tempfile::tempdir().unwrap();
    let engine = engine(&db, &ws, data.path()).await;
    index(&engine, &ws).await;
    write(
        &ws,
        &format!("{ROOT}/{SUBJECT}"),
        &format!("{SCOPE_TEXT}// {}\n", "padding".repeat(160)),
    );
    let head = ws.commit_all(PROJECT, "grow the synthetic allowed file");
    let impact = engine
        .analyze_impact(&alice_caller(), diff(Target::default(), &base, Some(&head)))
        .await
        .unwrap();
    assert!(
        impact
            .changed
            .iter()
            .any(|item| item.evidence.path.as_str() == SUBJECT),
        "{impact:?}"
    );
    assert!(
        !impact
            .changed
            .iter()
            .any(|item| item.name.contains("(removed)")),
        "{impact:?}"
    );
    assert!(
        impact
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::ExcludedByPolicy
                && gap.message.contains("too_large")),
        "{impact:?}"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn omitting_test_results_preserves_observed_test_risk_and_does_not_truncate_a_hidden_list() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let ws = workspace();
    for client in ["a", "b", "c"] {
        std::fs::remove_file(
            ws.project_dir(PROJECT)
                .join(format!("{ROOT}/src/client-{client}.ts")),
        )
        .unwrap();
    }
    for (label, value, expected) in [("first", 3, 8), ("second", 4, 10)] {
        write(
            &ws,
            &format!("{ROOT}/tests/scope-{label}.test.ts"),
            &format!(
                "import {{ ScopeProbe }} from '../src/scope';\nif (ScopeProbe({value}) !== {expected}) {{ throw new Error('synthetic scope assertion'); }}\n",
            ),
        );
    }
    let commit = ws.commit_all(
        PROJECT,
        "add synthetic test dependencies without ordinary clients",
    );
    let data = tempfile::tempdir().unwrap();
    let engine = engine(&db, &ws, data.path()).await;
    index(&engine, &ws).await;
    let caller = alice_caller();
    let before = engine.indexer().stats();
    let input = AnalyzeImpactInput {
        change: Some(ChangeSubject::Symbol {
            symbol: SymbolRef {
                symbol: Some("ScopeProbe".to_owned()),
                project: Some(name(PROJECT)),
                ..SymbolRef::default()
            },
        }),
        limit: Some(50),
        include_tests: Some(true),
        ..AnalyzeImpactInput::default()
    };
    let visible = engine.analyze_impact(&caller, input.clone()).await.unwrap();
    assert!(!visible.changed.is_empty(), "{visible:?}");
    assert!(visible.impacted.is_empty(), "{visible:?}");
    assert!(!visible.truncated, "{visible:?}");
    let test_paths: BTreeSet<_> = visible
        .tests
        .iter()
        .map(|item| item.evidence.path.as_str())
        .collect();
    assert_eq!(
        test_paths,
        BTreeSet::from(["tests/scope-first.test.ts", "tests/scope-second.test.ts"]),
    );
    assert!(
        visible
            .tests
            .iter()
            .all(|item| item.evidence.commit.as_str() == commit && !item.evidence.why.is_empty()),
        "{visible:?}",
    );
    assert!(
        visible.risk.as_ref().is_some_and(|risk| risk
            .factors
            .iter()
            .all(|factor| factor.code != RiskCode::UntestedCode)),
        "{visible:?}",
    );
    let hidden = engine
        .analyze_impact(
            &caller,
            AnalyzeImpactInput {
                include_tests: Some(false),
                ..input.clone()
            },
        )
        .await
        .unwrap();
    let mut visible_without_tests = visible.clone();
    visible_without_tests.tests.clear();
    assert_eq!(hidden, visible_without_tests);

    let limited = engine
        .analyze_impact(
            &caller,
            AnalyzeImpactInput {
                limit: Some(1),
                ..input.clone()
            },
        )
        .await
        .unwrap();
    assert_eq!(limited.tests.len(), 1, "{limited:?}");
    assert!(
        limited.truncated,
        "the requested test list is clipped: {limited:?}"
    );
    assert_eq!(limited.risk, visible.risk);
    let hidden_limited = engine
        .analyze_impact(
            &caller,
            AnalyzeImpactInput {
                include_tests: Some(false),
                limit: Some(1),
                ..input
            },
        )
        .await
        .unwrap();
    assert_eq!(hidden_limited, hidden);
    assert_eq!(
        engine.indexer().stats(),
        before,
        "impact queries must not queue work"
    );
}
