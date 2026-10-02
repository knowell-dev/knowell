//! Missing refs (no fallback) and embedding policy decisions.

use std::sync::Arc;

use knowell_config::DataPolicy;
use knowell_index::{EmbeddingPlan, Priority, SyncOutcome, TierSkip, TierState};
use knowell_store::views;

use crate::common::{
    CountingEmbedder, embed_with, fixture_workspace, git_available, indexer, require_db, view_of,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn missing_branch_fails_the_view_without_fallback() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&["contracts", "handbook"]));
    for project in &mut ws.resolved.projects {
        if project.name.as_str() == "contracts" {
            project.track.value = "branch:does-not-exist".parse().unwrap();
        } else {
            project.track.value = "branch:topic".parse().unwrap();
        }
    }
    ws.git("handbook", &["branch", "topic"]);
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, outcomes) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let missing = view_of(&registration, "contracts");
    let topic = view_of(&registration, "handbook");

    let SyncOutcome::Failed { reason, .. } = &outcomes[0] else {
        panic!("{outcomes:?}");
    };
    assert!(reason.contains("branch:does-not-exist"), "{reason}");
    assert!(reason.contains("no other ref"), "{reason}");
    let status = indexer.status(missing).await.unwrap();
    assert_eq!(status.active_generation, None);
    assert_eq!(status.active_commit, None);
    assert!(status.last_error.unwrap().contains("does-not-exist"));
    // Nothing was indexed in its place.
    let mut conn = db.conn().await;
    assert!(
        views::list_generations(&mut conn, missing)
            .await
            .unwrap()
            .iter()
            .all(|g| g.state == knowell_store::GenerationState::Failed)
    );
    // Asking again records no new failure.
    let again = indexer
        .refresh_view(missing, Priority::Interactive)
        .await
        .unwrap();
    assert!(matches!(again, SyncOutcome::Failed { .. }));
    assert_eq!(
        views::list_generations(&mut conn, missing)
            .await
            .unwrap()
            .len(),
        1
    );

    // The branch of a built view disappears: the view fails, and the data of
    // the last good generation keeps serving (not `main` in its place).
    let status = indexer.status(topic).await.unwrap();
    let served = status.active_commit.clone().unwrap();
    ws.git("handbook", &["branch", "-D", "topic"]);
    let outcome = indexer
        .refresh_view(topic, Priority::Interactive)
        .await
        .unwrap();
    assert!(matches!(outcome, SyncOutcome::Failed { .. }), "{outcome:?}");
    indexer.run_until_idle().await.unwrap();
    let status = indexer.status(topic).await.unwrap();
    assert_eq!(status.active_commit.as_deref(), Some(served.as_str()));
    assert!(status.last_error.unwrap().contains("branch:topic"));
    drop(conn);
    drop(indexer);
    // The restarted engine still reports the failure from the store.
    let restarted = crate::common::indexer(&db, data.path(), &embedder);
    restarted.register(&ws.resolved).await.unwrap();
    let status = restarted.status(missing).await.unwrap();
    assert!(status.last_error.unwrap().contains("does-not-exist"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn local_only_projects_never_reach_a_cloud_provider() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&["contracts", "handbook", "db-migrations"]));
    embed_with(&mut ws.resolved, "cloud");
    for project in &mut ws.resolved.projects {
        match project.name.as_str() {
            // local-only (the fixture's workspace default) + cloud provider
            "contracts" => assert_eq!(project.data_policy.value, DataPolicy::LocalOnly),
            // no provider at all
            "handbook" => project.embedding.provider = None,
            // cloud allowed
            _ => project.data_policy.value = DataPolicy::Cloud,
        }
    }
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();

    let plan = |project: &str| {
        registration
            .views
            .iter()
            .find(|v| v.project.as_str() == project)
            .unwrap()
            .embedding
            .clone()
    };
    assert_eq!(
        plan("contracts"),
        EmbeddingPlan::Skip {
            reason: TierSkip::DataPolicyLocalOnly
        }
    );
    assert_eq!(
        plan("handbook"),
        EmbeddingPlan::Skip {
            reason: TierSkip::NoProvider
        }
    );
    assert!(matches!(plan("db-migrations"), EmbeddingPlan::Embed { .. }));

    let contracts = indexer
        .status(view_of(&registration, "contracts"))
        .await
        .unwrap();
    assert_eq!(
        contracts.tiers.t2,
        TierState::Skipped {
            reason: TierSkip::DataPolicyLocalOnly
        }
    );
    assert_eq!(contracts.tiers.t1, TierState::Done);
    let handbook = indexer
        .status(view_of(&registration, "handbook"))
        .await
        .unwrap();
    assert_eq!(
        handbook.tiers.t2,
        TierState::Skipped {
            reason: TierSkip::NoProvider
        }
    );
    // Lexical and symbol search still work for skipped projects.
    assert!(handbook.active_generation.is_some());
    let migrations = indexer
        .status(view_of(&registration, "db-migrations"))
        .await
        .unwrap();
    assert_eq!(migrations.tiers.t2, TierState::Done);

    // Only db-migrations' inputs were ever sent; nothing of the local-only
    // project has a vector.
    let sent = embedder.inputs();
    assert!(sent > 0);
    assert_eq!(
        u64::try_from(db.count("SELECT count(*) FROM embedding").await).unwrap(),
        sent
    );
    let migrations_inputs = db
        .count(&format!(
            "SELECT count(DISTINCT c.prepared_input_hash) FROM chunk c
             JOIN file_version f ON f.content_hash = c.content_hash
             WHERE f.view_id = '{}'",
            view_of(&registration, "db-migrations")
        ))
        .await;
    assert!(sent <= u64::try_from(migrations_inputs).unwrap());
    for project in ["contracts", "handbook"] {
        let embedded = db
            .count(&format!(
                "SELECT count(*) FROM chunk c
                 JOIN file_version f ON f.content_hash = c.content_hash
                 WHERE f.view_id = '{}'
                   AND EXISTS (SELECT 1 FROM embedding e
                               WHERE e.prepared_input_hash = c.prepared_input_hash)",
                view_of(&registration, project)
            ))
            .await;
        assert_eq!(embedded, 0, "{project} has vectors");
    }
    // The status of a restarted engine derives the same tiers from config
    // and the store.
    drop(indexer);
    let restarted = crate::common::indexer(&db, data.path(), &embedder);
    restarted.register(&ws.resolved).await.unwrap();
    let again = restarted
        .status(view_of(&registration, "contracts"))
        .await
        .unwrap();
    assert_eq!(
        again.tiers.t2,
        TierState::Skipped {
            reason: TierSkip::DataPolicyLocalOnly
        }
    );
    let again = restarted
        .status(view_of(&registration, "db-migrations"))
        .await
        .unwrap();
    assert_eq!(again.tiers.t2, TierState::Done);
}
