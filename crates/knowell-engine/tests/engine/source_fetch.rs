use knowell_core::{LineRange, RepoPath, TrackTarget};
use knowell_index::Priority;
use knowell_mcp::tools::{FetchInput, OpenWorkspaceInput, VersionStatus};
use knowell_mcp::{FileLocator, GapReason, KnowellTools, Target};

use super::source_context::source_engine;
use crate::common::{
    alice_caller, fixture_workspace, git_available, indexed_engine, name, require_db,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn continuation_fetch_prioritizes_requested_source_over_surrounding_context() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let path = RepoPath::new("src/source-continuation-context.rs").unwrap();
    let original = "pub fn context_probe() {\n    first();\n    second();\n    third();\n}\n";
    std::fs::write(ws.project_dir("billing-api").join(path.as_str()), original).unwrap();
    let commit = ws.commit_all("billing-api", "add synthetic context continuation source");
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
    let second = engine
        .fetch(
            &caller,
            FetchInput {
                target: target.clone(),
                ids: first.continuation_ids.clone(),
                context_lines: Some(200),
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let second = second.items.first().unwrap();
    assert_eq!(second.evidence.lines, LineRange::new(3, 4).unwrap());
    assert_eq!(second.evidence.commit.as_str(), commit);
    assert_ne!(second.id, first.id);
    assert_ne!(second.continuation_ids, first.continuation_ids);
    let third = engine
        .fetch(
            &caller,
            FetchInput {
                target,
                ids: second.continuation_ids.clone(),
                context_lines: Some(200),
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let third = third.items.first().unwrap();
    assert_eq!(third.evidence.lines, LineRange::new(5, 5).unwrap());
    assert!(third.continuation_ids.is_empty());
    assert_eq!(
        format!(
            "{}{}{}",
            first.content.text(),
            second.content.text(),
            third.content.text()
        ),
        original
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn current_fetch_rejects_a_retained_commit_prefix_collision() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let path = RepoPath::new("src/source-prefix-collision.rs").unwrap();
    let original = "pub fn prefix_collision_probe() {}\n";
    std::fs::write(ws.project_dir("billing-api").join(path.as_str()), original).unwrap();
    let old_commit = ws.commit_all("billing-api", "add synthetic prefix collision source");
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let old = engine
        .fetch(
            &caller,
            FetchInput {
                target: Target::context(opened.context_id),
                paths: vec![FileLocator {
                    project: name("billing-api"),
                    path: path.clone(),
                    lines: None,
                }],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let old_id = old.items.first().unwrap().id.clone();
    std::fs::write(
        ws.project_dir("billing-api").join("prefix-revision.txt"),
        "synthetic revision\n",
    )
    .unwrap();
    let next_commit = ws.commit_all("billing-api", "advance synthetic prefix collision snapshot");
    engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let collision = format!(
        "{}{}",
        old_commit.chars().take(12).collect::<String>(),
        "b".repeat(28)
    );
    assert_ne!(collision, old_commit);
    let mut conn = db.store.acquire().await.unwrap();
    sqlx::query("UPDATE view_generation SET resolved_commit = $1 WHERE resolved_commit = $2")
        .bind(&collision)
        .bind(&next_commit)
        .execute(&mut *conn)
        .await
        .unwrap();
    sqlx::query("UPDATE view SET active_commit = $1 WHERE active_commit = $2")
        .bind(&collision)
        .bind(&next_commit)
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    let ambiguous = engine
        .fetch(
            &caller,
            FetchInput {
                target: target.clone(),
                ids: vec![old_id],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(ambiguous.items.is_empty());
    assert!(
        ambiguous
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::NotFound
                && gap.message.contains("ambiguous across retained commits"))
    );
    let exact_path = engine
        .fetch(
            &caller,
            FetchInput {
                target,
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
    assert_eq!(exact_path.items.first().unwrap().content.text(), original);
    assert_eq!(
        exact_path.items.first().unwrap().evidence.commit.as_str(),
        collision
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn historical_fetch_keeps_the_requested_commit_after_update_and_delete() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == "billing-api");
    let path = RepoPath::new("src/source-history.ts").unwrap();
    let file = ws.project_dir("billing-api").join(path.as_str());
    let old_body = "export function historyProbe(): number {\n  return 1;\n}\n";
    std::fs::write(&file, old_body).unwrap();
    let old_commit = ws.commit_all("billing-api", "add synthetic historical source");
    let data = tempfile::tempdir().unwrap();
    let engine = indexed_engine(&db, &ws, data.path()).await;
    // An upgraded index may have source bodies but no persisted line counts.
    let mut conn = db.store.acquire().await.unwrap();
    sqlx::query("UPDATE content SET redacted_line_count = NULL")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let caller = alice_caller();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let old = engine
        .fetch(
            &caller,
            FetchInput {
                target: Target::context(opened.context_id),
                paths: vec![FileLocator {
                    project: name("billing-api"),
                    path: path.clone(),
                    lines: None,
                }],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let original = old.items.first().unwrap();
    assert_eq!(original.evidence.commit.as_str(), old_commit);
    let id = original.id.clone();
    let old_hash = original.evidence.content_hash;
    assert_eq!(original.evidence.lines, LineRange::new(1, 3).unwrap());
    assert_eq!(original.content.text(), old_body);

    std::fs::write(
        ws.project_dir("billing-api").join("history-note.txt"),
        "synthetic note\n",
    )
    .unwrap();
    ws.commit_all(
        "billing-api",
        "advance synthetic snapshot without changing source",
    );
    engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let unchanged = engine
        .fetch(
            &caller,
            FetchInput {
                target: Target::context(opened.context_id),
                ids: vec![id.clone()],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let same_body = unchanged.items.first().unwrap();
    assert_eq!(same_body.status, VersionStatus::Changed);
    assert_eq!(same_body.evidence.commit.as_str(), old_commit);
    assert_eq!(same_body.evidence.content_hash, old_hash);
    assert_eq!(same_body.content.text(), old_body);

    std::fs::write(&file, old_body.replace("return 1", "return 2")).unwrap();
    ws.commit_all("billing-api", "change synthetic historical source");
    engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let mut conn = db.store.acquire().await.unwrap();
    sqlx::query("UPDATE content SET redacted_line_count = NULL")
        .execute(&mut *conn)
        .await
        .unwrap();
    drop(conn);
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let target = Target::context(opened.context_id);
    let changed = engine
        .fetch(
            &caller,
            FetchInput {
                target: target.clone(),
                ids: vec![id.clone()],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let historical = changed.items.first().unwrap();
    assert_eq!(historical.status, VersionStatus::Changed);
    assert_eq!(historical.evidence.commit.as_str(), old_commit);
    assert_eq!(
        historical.evidence.view,
        TrackTarget::Commit(old_commit.clone())
    );
    assert_eq!(historical.evidence.content_hash, old_hash);
    assert_eq!(historical.content.text(), old_body);
    assert!(historical.current_id.is_some());

    let current = engine
        .fetch(
            &caller,
            FetchInput {
                target: target.clone(),
                ids: vec![historical.current_id.clone().unwrap()],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let current = current.items.first().unwrap();
    assert_eq!(current.status, VersionStatus::Current);
    assert_eq!(current.evidence.lines, LineRange::new(1, 3).unwrap());
    assert_eq!(
        current.content.text(),
        old_body.replace("return 1", "return 2")
    );

    let outside = engine
        .fetch(
            &caller,
            FetchInput {
                target,
                paths: vec![FileLocator {
                    project: name("billing-api"),
                    path: path.clone(),
                    lines: Some(LineRange::new(999, 1000).unwrap()),
                }],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    assert!(outside.items.is_empty());
    assert!(
        outside
            .gaps
            .iter()
            .any(|gap| gap.reason == GapReason::NotFound)
    );

    std::fs::remove_file(&file).unwrap();
    ws.commit_all("billing-api", "delete synthetic historical source");
    engine
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let opened = engine
        .open_workspace(&caller, OpenWorkspaceInput::default())
        .await
        .unwrap();
    let deleted = engine
        .fetch(
            &caller,
            FetchInput {
                target: Target::context(opened.context_id),
                ids: vec![id],
                ..FetchInput::default()
            },
        )
        .await
        .unwrap();
    let historical = deleted.items.first().unwrap();
    assert_eq!(historical.status, VersionStatus::Deleted);
    assert_eq!(historical.evidence.commit.as_str(), old_commit);
    assert_eq!(historical.evidence.content_hash, old_hash);
    assert_eq!(historical.content.text(), old_body);
    assert!(historical.current_id.is_none());
}
