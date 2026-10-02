//! Several indexers sharing one queue claim only the jobs of their views.

use std::sync::Arc;

use knowell_index::{Priority, SyncOutcome, TierState};

use crate::common::{
    CountingEmbedder, fixture_workspace, git_available, indexer, require_db, view_of,
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
