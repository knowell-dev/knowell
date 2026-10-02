//! The generation fence against late jobs, and crash recovery through
//! lease expiry.

use std::sync::Arc;
use std::time::Duration;

use knowell_index::jobs::{JOB_KINDS, JOB_TEXT};
use knowell_index::{Priority, ProgressKind, SyncOutcome};
use knowell_store::jobs;
use knowell_store::views;
use knowell_store::{GenerationState, JobState, StoreError};

use crate::common::{
    CountingEmbedder, config, embed_with, fixture_workspace, git_available, indexer, indexer_with,
    require_db, view_of,
};

const PROJECT: &str = "contracts";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_late_older_build_never_activates_over_a_newer_one() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&[PROJECT]));
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let mut events = indexer.subscribe();

    // Build of C1 runs up to (not including) its activation stage: T0 and
    // T1 (T3 activates; T2 follows activation).
    ws.write(PROJECT, "notes/first.md", "# First\n\nfirst change\n");
    let c1 = ws.commit_all(PROJECT, "first");
    let queued = indexer
        .refresh_view(view, Priority::Background)
        .await
        .unwrap();
    assert!(matches!(queued, SyncOutcome::Queued { .. }));
    for _ in 0..2 {
        assert!(indexer.run_next_job().await.unwrap());
    }
    let mut conn = db.conn().await;
    let g_old = views::building_generation(&mut conn, view)
        .await
        .unwrap()
        .expect("C1 is building");

    // C2 lands; its interactive build overtakes the queued T3 of C1.
    ws.write(PROJECT, "notes/second.md", "# Second\n\nsecond change\n");
    let c2 = ws.commit_all(PROJECT, "second");
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    let summary = indexer.run_until_idle().await.unwrap();
    assert_eq!(summary.failed, 0, "{summary:?}");

    let row = views::get_view(&mut conn, view).await.unwrap().unwrap();
    assert_eq!(row.active_commit.as_deref(), Some(c2.as_str()));
    let g_new = row.active_generation.unwrap();
    assert!(g_new > g_old);
    let old = views::get_generation(&mut conn, view, g_old)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(old.state, GenerationState::Failed);
    assert!(old.error.unwrap().contains("superseded"));
    assert_ne!(old.resolved_commit.as_deref(), Some(c2.as_str()));
    assert_eq!(old.resolved_commit.as_deref(), Some(c1.as_str()));

    // The late T3 of C1 ended quietly (succeeded, not failed or dead).
    let counts = jobs::job_counts(&mut conn).await.unwrap();
    assert!(
        counts
            .iter()
            .all(|c| matches!(c.state, JobState::Succeeded)),
        "{counts:?}"
    );
    let mut superseded = false;
    while let Ok(event) = events.try_recv() {
        if event.generation == Some(g_old) && matches!(event.kind, ProgressKind::Superseded { .. })
        {
            superseded = true;
        }
        assert!(
            !(event.generation == Some(g_old)
                && matches!(event.kind, ProgressKind::Activated { .. })),
            "the old generation must never activate"
        );
    }
    assert!(superseded);
    assert!(indexer.stats().builds_superseded >= 1);

    // The fence itself: activating the old generation now is refused.
    let refused = views::activate_generation(&mut conn, view, g_old).await;
    assert!(
        matches!(
            refused,
            Err(StoreError::StaleGeneration { .. } | StoreError::GenerationNotBuilding { .. })
        ),
        "{refused:?}"
    );
    let row = views::get_view(&mut conn, view).await.unwrap().unwrap();
    assert_eq!(row.active_generation, Some(g_new));
    drop(conn);
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn crashed_workers_jobs_are_reclaimed_and_resumed() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&[PROJECT]));
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer_with(&db, config(data.path()), &embedder);
    let registration = indexer.register(&ws.resolved).await.unwrap();
    let view = view_of(&registration, PROJECT);
    let commit = ws.head(PROJECT);
    let queued = indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    assert!(matches!(queued, SyncOutcome::Queued { .. }));

    // A worker claims the T0 job with a short lease, starts the generation
    // and dies.
    let mut conn = db.conn().await;
    let job = jobs::claim(
        &mut conn,
        "crashed-worker",
        &JOB_KINDS,
        Duration::from_millis(500),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(job.kind, JOB_TEXT);
    let started = views::begin_generation(&mut conn, view, Some(&commit))
        .await
        .unwrap();
    // While the lease holds, nobody else gets the job.
    assert!(!indexer.run_next_job().await.unwrap());
    tokio::time::sleep(Duration::from_millis(800)).await;

    // A new worker recovers the job and resumes the same generation.
    let summary = indexer.run_until_idle().await.unwrap();
    assert!(summary.jobs >= 4, "{summary:?}");
    assert_eq!(summary.failed, 0);
    let job = jobs::get_job(&mut conn, job.id).await.unwrap().unwrap();
    assert_eq!(job.state, JobState::Succeeded);
    assert_eq!(job.attempts, 2);
    let row = views::get_view(&mut conn, view).await.unwrap().unwrap();
    assert_eq!(
        row.active_generation,
        Some(started),
        "the crashed generation was resumed"
    );
    assert_eq!(row.last_generation, started);
    assert_eq!(row.active_commit.as_deref(), Some(commit.as_str()));
    drop(conn);
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn reconciliation_repairs_lost_lexical_indexes_and_store_rows() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&[PROJECT]));
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let reports = indexer.reconcile().await.unwrap();
    assert_eq!(reports[0].store_consistent, Some(true));
    drop(indexer);

    // The engine was down and its lexical indexes were lost.
    std::fs::remove_dir_all(data.path().join("lexical")).unwrap();
    let indexer = crate::common::indexer(&db, data.path(), &embedder);
    indexer.register(&ws.resolved).await.unwrap();
    let reports = indexer.reconcile().await.unwrap();
    assert!(reports[0].lexical_restored, "{reports:?}");
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    assert!(lexical.num_docs() > 0);
    drop(lexical);

    // Rows of the active generation vanished from the store (for example a
    // partial restore): the tree hash no longer matches, so it is rebuilt.
    let mut conn = db.conn().await;
    let row = views::get_view(&mut conn, view).await.unwrap().unwrap();
    let active = row.active_generation.unwrap();
    sqlx::query(
        "DELETE FROM file_version WHERE view_id = $1
         AND path = (SELECT min(path) FROM file_version WHERE view_id = $1)",
    )
    .bind(view)
    .execute(&mut *conn)
    .await
    .unwrap();
    let reports = indexer.reconcile().await.unwrap();
    assert_eq!(reports[0].store_consistent, Some(false), "{reports:?}");
    assert!(reports[0].rebuild_queued);
    indexer.run_until_idle().await.unwrap();
    let row = views::get_view(&mut conn, view).await.unwrap().unwrap();
    assert!(row.active_generation.unwrap() > active);
    let reports = indexer.reconcile().await.unwrap();
    assert_eq!(reports[0].store_consistent, Some(true), "{reports:?}");
    drop(conn);
    drop(indexer);
}
