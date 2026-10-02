//! Watcher-triggered refreshes and reconciliation.

use std::sync::Arc;

use knowell_index::{Priority, SyncOutcome, Worker, WorkerConfig};
use tokio_util::sync::CancellationToken;

use crate::common::{
    CountingEmbedder, active_pin, fixture_workspace, git_available, indexer, path, require_db,
    view_of, wait_for,
};

const PROJECT: &str = "handbook";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commits_and_saves_reach_the_index_through_the_watcher() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&[PROJECT]));
    for project in &mut ws.resolved.projects {
        project.track.value = knowell_core::TrackTarget::WorktreeHead;
    }
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);

    let shutdown = CancellationToken::new();
    let worker = Worker::new(&indexer, WorkerConfig::default()).spawn(shutdown.clone());
    let watch = indexer.watch(shutdown.clone()).unwrap();
    assert_eq!(watch.watched().len(), 1);

    // A commit moves HEAD: the view follows without an explicit refresh.
    ws.write(
        PROJECT,
        "docs/watched.md",
        "# Watched\n\nknowellWatchProbe committed\n",
    );
    let commit = ws.commit_all(PROJECT, "watched commit");
    wait_for("the watcher-triggered build", || {
        let indexer = indexer.clone();
        let commit = commit.clone();
        async move {
            indexer
                .status(view)
                .await
                .is_ok_and(|s| s.active_commit.as_deref() == Some(commit.as_str()))
        }
    })
    .await;
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    assert_eq!(lexical.search("knowellWatchProbe", 5).unwrap().len(), 1);

    // A saved, uncommitted file updates the personal overlay only.
    ws.write(PROJECT, "docs/draft.md", "# Draft\n\nknowellSavedProbe\n");
    let draft = path("docs/draft.md");
    wait_for("the overlay update", || {
        let indexer = indexer.clone();
        let draft = draft.clone();
        async move {
            indexer
                .overlay(view)
                .is_some_and(|o| o.shadowed_paths().contains(&draft))
        }
    })
    .await;
    let overlay = indexer.overlay(view).unwrap();
    assert_eq!(overlay.search("knowellSavedProbe", 5).unwrap().len(), 1);
    let pin = active_pin(&db, view).await;
    let mut conn = db.conn().await;
    assert!(
        knowell_store::content::file_at(&mut conn, pin, &draft)
            .await
            .unwrap()
            .is_none()
    );
    drop(conn);

    // Reconciliation finds everything consistent.
    let reports = indexer.reconcile().await.unwrap();
    assert_eq!(reports.len(), 1);
    assert!(
        matches!(reports[0].sync, SyncOutcome::UpToDate { .. }),
        "{reports:?}"
    );
    assert_eq!(reports[0].store_consistent, Some(true));
    assert!(!reports[0].lexical_restored);

    shutdown.cancel();
    watch.stop().await;
    worker.await.unwrap().unwrap();
    drop(lexical);
    drop(indexer);
}
