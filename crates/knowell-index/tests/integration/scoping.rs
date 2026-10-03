//! Several indexers sharing one queue claim only the jobs of their views.

use std::sync::Arc;
use std::time::Duration;

use knowell_index::jobs::{JOB_SYNC, JOB_TEXT};
use knowell_index::{Priority, SyncOutcome, TierState};
use knowell_store::JobState;
use knowell_store::jobs::{self, ClaimScope, JobScope, NewJob};

use crate::common::{
    CountingEmbedder, fixture_workspace, git_available, indexer, name, require_db, view_of,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn indexers_sharing_a_queue_never_take_each_others_jobs() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&["contracts", "handbook"]));
    let mut contracts_only = ws.resolved.clone();
    contracts_only
        .projects
        .retain(|p| p.name.as_str() == "contracts");
    let mut handbook_only = ws.resolved.clone();
    handbook_only
        .projects
        .retain(|p| p.name.as_str() == "handbook");
    let (data_a, data_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let embedder = Arc::new(CountingEmbedder::new());
    let a = indexer(&db, data_a.path(), &embedder);
    let b = indexer(&db, data_b.path(), &embedder);
    let contracts = view_of(&a.register(&contracts_only).await.unwrap(), "contracts");
    let handbook = view_of(&b.register(&handbook_only).await.unwrap(), "handbook");

    let queued = a
        .refresh_view(contracts, Priority::Interactive)
        .await
        .unwrap();
    assert!(matches!(queued, SyncOutcome::Queued { .. }));
    // The job is attributed to its view.
    assert_eq!(
        db.count(&format!(
            "SELECT count(*) FROM job WHERE view_id = '{contracts}' AND workspace_id IS NOT NULL"
        ))
        .await,
        1
    );
    // B does not serve `contracts`: it leaves the job alone.
    let summary = b.run_until_idle().await.unwrap();
    assert_eq!(summary.jobs, 0, "{summary:?}");
    assert_eq!(
        db.count("SELECT count(*) FROM job WHERE state = 'queued'")
            .await,
        1
    );

    b.refresh_view(handbook, Priority::Interactive)
        .await
        .unwrap();
    // A runs only its own build, B then runs its own.
    let ran_a = a.run_until_idle().await.unwrap();
    assert!(ran_a.jobs >= 4 && ran_a.failed == 0, "{ran_a:?}");
    assert_eq!(
        db.count(&format!(
            "SELECT count(*) FROM job WHERE view_id = '{handbook}' AND state = 'queued'"
        ))
        .await,
        1,
        "the other view's job is still waiting"
    );
    let ran_b = b.run_until_idle().await.unwrap();
    assert!(ran_b.jobs >= 4 && ran_b.failed == 0, "{ran_b:?}");
    for (indexer, view) in [(&a, contracts), (&b, handbook)] {
        let status = indexer.status(view).await.unwrap();
        assert!(status.active_generation.is_some());
        assert_eq!(status.tiers.t3, TierState::Done);
        assert_eq!(status.last_error, None);
    }
    assert_eq!(
        db.count("SELECT count(*) FROM job WHERE state IN ('dead', 'failed', 'queued')")
            .await,
        0,
        "nothing was dead-lettered or left behind"
    );
    drop((a, b));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn strict_runner_recovers_and_runs_only_its_registered_views() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&["contracts", "handbook"]));
    let mut selected = ws.resolved.clone();
    selected.projects.retain(|p| p.name.as_str() == "contracts");
    let mut foreign = ws.resolved.clone();
    foreign.name = name("foreign-workspace");
    foreign.projects.retain(|p| p.name.as_str() == "handbook");
    let (data_a, data_b) = (tempfile::tempdir().unwrap(), tempfile::tempdir().unwrap());
    let embedder = Arc::new(CountingEmbedder::new());
    let a = indexer(&db, data_a.path(), &embedder);
    let b = indexer(&db, data_b.path(), &embedder);
    let own_view = view_of(&a.register(&selected).await.unwrap(), "contracts");
    let foreign_view = view_of(&b.register(&foreign).await.unwrap(), "handbook");
    let SyncOutcome::Queued { job: own_job, .. } = a
        .refresh_view(own_view, Priority::Interactive)
        .await
        .unwrap()
    else {
        panic!("the selected synthetic target must queue a build");
    };
    let SyncOutcome::Queued {
        job: foreign_queued,
        ..
    } = b
        .refresh_view(foreign_view, Priority::Interactive)
        .await
        .unwrap()
    else {
        panic!("the foreign synthetic target must queue a build");
    };
    let mut c = db.conn().await;
    let mut legacy = NewJob::new(
        JOB_SYNC,
        serde_json::json!({
            "view": foreign_view,
            "priority": "interactive",
            "force": false,
        }),
    );
    legacy.priority = i32::MAX;
    let foreign_unscoped = jobs::enqueue(&mut c, &legacy).await.unwrap().id;
    let foreign_running = jobs::enqueue_scoped(&mut c, &legacy, JobScope::View(foreign_view))
        .await
        .unwrap()
        .id;
    let own_scope = ClaimScope {
        views: vec![own_view],
        ..ClaimScope::default()
    };
    let foreign_scope = ClaimScope {
        views: vec![foreign_view],
        ..ClaimScope::default()
    };
    for (scope, kind, expected) in [
        (&own_scope, JOB_TEXT, own_job),
        (&foreign_scope, JOB_SYNC, foreign_running),
    ] {
        let claimed = jobs::claim_scoped(
            &mut c,
            "crashed-synthetic-worker",
            &[kind],
            Duration::from_secs(30),
            scope,
        )
        .await
        .unwrap()
        .unwrap();
        assert_eq!(claimed.id, expected);
        sqlx::query("UPDATE job SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
            .bind(expected)
            .execute(&mut *c)
            .await
            .unwrap();
    }
    let mut before = Vec::new();
    for id in [foreign_queued, foreign_unscoped, foreign_running] {
        before.push(jobs::get_job(&mut c, id).await.unwrap().unwrap());
    }
    drop(c);
    let summary = a.run_until_idle_scoped_with(4).await.unwrap();
    assert!(summary.jobs >= 4 && summary.failed == 0, "{summary:?}");
    let status = a.status(own_view).await.unwrap();
    assert_eq!(
        status.active_commit.as_deref(),
        Some(ws.head("contracts").as_str())
    );
    assert!(status.active_generation.is_some());
    assert_eq!(status.tiers.t3, TierState::Done);
    assert_eq!(status.last_error, None);
    let mut c = db.conn().await;
    for previous in before {
        let current = jobs::get_job(&mut c, previous.id).await.unwrap().unwrap();
        assert_eq!(
            current, previous,
            "foreign state, attempts and lease remain unchanged"
        );
    }
    let recovered = jobs::get_job(&mut c, own_job).await.unwrap().unwrap();
    assert_eq!(recovered.state, JobState::Succeeded);
    assert_eq!(
        recovered.attempts, 2,
        "the selected expired lease was recovered"
    );
    assert_eq!(
        b.status(foreign_view).await.unwrap().active_generation,
        None
    );
    drop((c, a, b));
}
