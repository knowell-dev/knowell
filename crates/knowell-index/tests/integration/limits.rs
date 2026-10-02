//! Provider outages, budgets, count limits and plain-directory sources.

use std::sync::Arc;
use std::time::Duration;

use knowell_index::{Indexer, Priority, SyncOutcome, TierSkip, TierState};

use crate::common::{
    CountingEmbedder, TestDb, active_files, config, embed_with, fixture_workspace, git_available,
    indexer, indexer_with, require_db, view_of,
};

/// Runs the queue until nothing is left, including jobs waiting for a
/// (short) retry delay.
async fn drain(indexer: &Indexer<CountingEmbedder>, db: &TestDb) {
    for _ in 0..300 {
        indexer.run_until_idle().await.unwrap();
        let pending = db
            .count("SELECT count(*) FROM job WHERE state IN ('queued', 'running', 'failed')")
            .await;
        if pending == 0 {
            return;
        }
        tokio::time::sleep(Duration::from_millis(30)).await;
    }
    panic!("the queue did not drain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_provider_outage_retries_then_reports_without_blocking_activation() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&["contracts"]));
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let mut settings = config(data.path());
    settings.jobs.max_attempts = 3;
    let indexer = indexer_with(&db, settings, &embedder);
    let registration = indexer.register(&ws.resolved).await.unwrap();
    let view = view_of(&registration, "contracts");

    embedder.set_down(true);
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    drain(&indexer, &db).await;
    let status = indexer.status(view).await.unwrap();
    // Text and symbols are served; the vector tier says why it is missing.
    assert_eq!(
        status.active_commit.as_deref(),
        Some(ws.head("contracts").as_str())
    );
    assert_eq!(status.tiers.t0, TierState::Done);
    assert_eq!(status.tiers.t1, TierState::Done);
    let TierState::Failed { reason } = &status.tiers.t2 else {
        panic!("{:?}", status.tiers);
    };
    assert!(reason.contains("timed out"), "{reason}");
    assert_eq!(status.tiers.t3, TierState::Done);
    // The view is active and searchable; semantic coverage says it is not.
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    assert!(lexical.num_docs() > 0);
    drop(lexical);
    let coverage = indexer.embedding_coverage(view).await.unwrap().unwrap();
    assert!(coverage.is_partial() && !coverage.complete, "{coverage:?}");
    assert_eq!(coverage.embedded, 0);
    assert_eq!(embedder.calls(), 3, "one call per attempt");
    assert_eq!(
        db.count(
            "SELECT count(*) FROM job WHERE kind = 'index.embeddings' AND attempts = 3 AND state = 'succeeded'"
        )
        .await,
        1
    );
    assert_eq!(
        db.count("SELECT count(*) FROM index_generation WHERE state = 'active'")
            .await,
        0,
        "incomplete vectors are never presented as current"
    );

    // The provider recovers; the next build embeds everything that is
    // missing, not only the changed file.
    embedder.set_down(false);
    ws.write(
        "contracts",
        "notes/recovered.md",
        "# Recovered\n\nprovider is back\n",
    );
    ws.commit_all("contracts", "after the outage");
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    drain(&indexer, &db).await;
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.tiers.t2, TierState::Done, "{:?}", status.tiers);
    assert!(embedder.inputs() > 5, "{} inputs", embedder.inputs());
    assert_eq!(
        db.count("SELECT count(*) FROM index_generation WHERE state = 'active'")
            .await,
        1
    );
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn budgets_and_limits_are_reported_not_truncated() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&["contracts", "handbook"]));
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let mut settings = config(data.path());
    settings.embedding.budget = Some(knowell_embed::Budget::new(Some(10), None, 0.0).unwrap());
    settings.jobs.max_attempts = 1;
    // contracts has fewer files than this, handbook more.
    let contracts_files = ws
        .manifest
        .projects
        .iter()
        .find(|p| p.name.as_str() == "contracts")
        .unwrap()
        .files
        .len();
    let handbook_files = ws
        .manifest
        .projects
        .iter()
        .find(|p| p.name.as_str() == "handbook")
        .unwrap()
        .files
        .len();
    assert!(contracts_files < handbook_files);
    settings.limits.max_files_per_view = contracts_files;
    let indexer = indexer_with(&db, settings, &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    drain(&indexer, &db).await;

    // contracts fits the file limit; its inputs do not fit the budget.
    let contracts = indexer
        .status(view_of(&registration, "contracts"))
        .await
        .unwrap();
    assert!(contracts.active_generation.is_some(), "{contracts:?}");
    assert_eq!(
        contracts.tiers.t2,
        TierState::Skipped {
            reason: TierSkip::BudgetExhausted
        }
    );
    assert_eq!(embedder.calls(), 0, "nothing is sent beyond the budget");

    // handbook has more files than allowed: the build is dead-lettered and
    // the view says why; nothing partial is activated.
    let handbook = indexer
        .status(view_of(&registration, "handbook"))
        .await
        .unwrap();
    assert_eq!(handbook.active_generation, None);
    let error = handbook.last_error.unwrap();
    assert!(error.contains("max_files_per_view"), "{error}");
    assert_eq!(
        db.count("SELECT count(*) FROM job WHERE state = 'dead'")
            .await,
        1
    );
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn plain_directories_are_indexed_and_reconciled() {
    let db = require_db!();
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("ws");
    let notes = root.join("notes");
    std::fs::create_dir_all(notes.join("node_modules/pkg")).unwrap();
    std::fs::write(
        notes.join("a.md"),
        "# Alpha\n\nknowellDirectoryProbe alpha\n",
    )
    .unwrap();
    std::fs::write(notes.join("b.py"), "def beta():\n    return 1\n").unwrap();
    std::fs::write(notes.join(".env"), "PASSWORD=never-read\n").unwrap();
    std::fs::write(
        notes.join("node_modules/pkg/index.js"),
        "module.exports = 1;\n",
    )
    .unwrap();
    let workspace = knowell_config::parse_workspace(
        "version = 1\n[workspace]\nname = \"plain\"\ntrack = \"worktree\"\n[[project]]\nname = \"notes\"\npath = \"notes\"\n",
    )
    .unwrap();
    let resolved = workspace.resolve(&root).unwrap();
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    assert_eq!(
        registration.views[0].source_kind,
        knowell_store::SourceKind::Directory
    );
    let view = registration.views[0].view;
    let files = active_files(&db, view).await;
    let names: Vec<&str> = files.keys().map(|p| p.as_str()).collect();
    assert_eq!(names, vec!["a.md", "b.py"]);
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.active_commit, None);
    assert_eq!(status.tiers.t1, TierState::Done);

    // Unchanged: nothing to do. Changed on disk: reconciliation rebuilds.
    let reports = indexer.reconcile().await.unwrap();
    assert!(
        matches!(reports[0].sync, SyncOutcome::UpToDate { .. }),
        "{reports:?}"
    );
    std::fs::write(
        notes.join("c.md"),
        "# Gamma\n\nknowellDirectoryProbe gamma\n",
    )
    .unwrap();
    std::fs::remove_file(notes.join("b.py")).unwrap();
    let reports = indexer.reconcile().await.unwrap();
    assert!(
        matches!(reports[0].sync, SyncOutcome::Queued { .. }),
        "{reports:?}"
    );
    indexer.run_until_idle().await.unwrap();
    let files = active_files(&db, view).await;
    let names: Vec<&str> = files.keys().map(|p| p.as_str()).collect();
    assert_eq!(names, vec!["a.md", "c.md"]);
    let hits = indexer
        .lexical(view)
        .await
        .unwrap()
        .unwrap()
        .search("knowellDirectoryProbe", 5)
        .unwrap();
    assert_eq!(hits.len(), 2);
    drop(indexer);
}
