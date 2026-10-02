//! Searchable in seconds: a build activates after text, symbols and
//! relations; embeddings follow as enrichment of the active generation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::time::{Duration, Instant};

use knowell_index::{Priority, ProgressKind, SyncOutcome, Tier, TierState};
use knowell_store::ViewId;
use tokio::sync::broadcast::error::RecvError;

use crate::common::{
    CountingEmbedder, config, embed_with, fixture_workspace, git_available, indexer, indexer_with,
    require_db, view_of,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn lexical_and_symbol_search_work_before_embeddings_finish() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&["billing-api"]));
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let registration = indexer.register(&ws.resolved).await.unwrap();
    let view = view_of(&registration, "billing-api");
    let queued = indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    assert!(matches!(queued, SyncOutcome::Queued { .. }));

    // T0, T1 and T3 make the generation active; T2 has not run.
    let mut steps = 0;
    while indexer
        .status(view)
        .await
        .unwrap()
        .active_generation
        .is_none()
    {
        assert!(indexer.run_next_job().await.unwrap(), "the build stalled");
        steps += 1;
        assert!(steps <= 3, "activation needs T0, T1 and T3 only");
    }
    assert_eq!(steps, 3);
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.building_generation, None);
    assert_eq!(status.tiers.t0, TierState::Done);
    assert_eq!(status.tiers.t1, TierState::Done);
    assert_eq!(status.tiers.t3, TierState::Done);
    assert_eq!(status.tiers.t2, TierState::Pending, "{:?}", status.tiers);
    assert_eq!(embedder.calls(), 0, "nothing was embedded yet");

    // Lexical and symbol search already serve the new generation.
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    assert!(!lexical.search("cancel subscription", 5).unwrap().is_empty());
    drop(lexical);
    let definitions = db
        .count(&format!(
            "SELECT count(*) FROM occurrence WHERE view_id = '{view}' AND role = 'definition'"
        ))
        .await;
    assert!(definitions > 10, "{definitions} definitions");

    // Semantic coverage is reported as partial, never as current.
    let coverage = indexer.embedding_coverage(view).await.unwrap().unwrap();
    assert_eq!(coverage.generation, status.active_generation.unwrap());
    assert!(coverage.inputs > 0);
    assert_eq!(coverage.embedded, 0);
    assert!(coverage.is_partial() && !coverage.complete);

    // Another process sees the pending T2 from the store.
    let observer = crate::common::indexer(&db, data.path(), &embedder);
    observer.register(&ws.resolved).await.unwrap();
    assert_eq!(
        observer.status(view).await.unwrap().tiers.t2,
        TierState::Pending
    );

    indexer.run_until_idle().await.unwrap();
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.tiers.t2, TierState::Done, "{:?}", status.tiers);
    let coverage = indexer.embedding_coverage(view).await.unwrap().unwrap();
    assert!(coverage.complete);
    assert_eq!(coverage.embedded, coverage.inputs);
    assert!(embedder.inputs() > 0);
    assert_eq!(
        observer.status(view).await.unwrap().tiers.t2,
        TierState::Done
    );
    drop(observer);
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn embeddings_of_a_superseded_generation_stop_early() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&["contracts"]));
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, "contracts");

    // C1 is built and activated; its T2 waits in the queue.
    ws.write(
        "contracts",
        "notes/one.md",
        "# One\n\nknowellSupersededProbe one\n",
    );
    ws.commit_all("contracts", "one");
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    for _ in 0..3 {
        assert!(indexer.run_next_job().await.unwrap());
    }
    let g_one = indexer
        .status(view)
        .await
        .unwrap()
        .active_generation
        .unwrap();
    // C2 lands; its T0..T3 run before the queued T2 of C1 (lower priority).
    ws.write(
        "contracts",
        "notes/two.md",
        "# Two\n\nknowellSupersededProbe two\n",
    );
    let c2 = ws.commit_all("contracts", "two");
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    for _ in 0..3 {
        assert!(indexer.run_next_job().await.unwrap());
    }
    let status = indexer.status(view).await.unwrap();
    let g_two = status.active_generation.unwrap();
    assert!(g_two > g_one);
    assert_eq!(status.active_commit.as_deref(), Some(c2.as_str()));
    // Both generations' texts are searchable without any vector.
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    assert_eq!(
        lexical.search("knowellSupersededProbe", 5).unwrap().len(),
        2
    );
    drop(lexical);

    // The T2 of C1 finds its generation replaced and ends without a call.
    let calls = embedder.calls();
    let mut events = indexer.subscribe();
    assert!(indexer.run_next_job().await.unwrap());
    assert_eq!(embedder.calls(), calls);
    let mut superseded = false;
    while let Ok(event) = events.try_recv() {
        if event.generation == Some(g_one) && matches!(event.kind, ProgressKind::Superseded { .. })
        {
            superseded = true;
        }
    }
    assert!(superseded);
    assert_eq!(
        db.count(&format!(
            "SELECT count(*) FROM index_generation WHERE view_id = '{view}' AND view_generation = {g_one} AND state = 'active'"
        ))
        .await,
        0
    );

    // The T2 of C2 embeds both new files' inputs and reuses everything else.
    let inputs = embedder.inputs();
    let reused = indexer.stats().inputs_reused;
    indexer.run_until_idle().await.unwrap();
    let embedded = embedder.inputs() - inputs;
    assert!(embedded >= 2, "{embedded} inputs embedded");
    assert!(indexer.stats().inputs_reused > reused);
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.tiers.t2, TierState::Done);
    let coverage = indexer.embedding_coverage(view).await.unwrap().unwrap();
    assert!(coverage.complete && !coverage.is_partial());
    // Every job ended without failing.
    assert_eq!(
        db.count("SELECT count(*) FROM job WHERE state <> 'succeeded'")
            .await,
        0
    );
    drop(indexer);
}

/// Time until every view of the Small fixture is searchable (T0, T1, T3 and
/// activation) compared with the time until its embeddings are done, with a
/// simulated provider latency. Prints the numbers; asserts the order.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn time_to_searchable_on_the_small_fixture() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(None);
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    // A remote provider: 40 ms per call of at most 16 inputs.
    embedder.set_delay(Duration::from_millis(40));
    let mut settings = config(data.path());
    settings.embedding.batch_size = 16;
    let indexer = indexer_with(&db, settings, &embedder);
    let registration = indexer.register(&ws.resolved).await.unwrap();
    let views: BTreeSet<ViewId> = registration.views.iter().map(|v| v.view).collect();
    let mut events = indexer.subscribe();
    let started = Instant::now();
    for view in &views {
        indexer
            .refresh_view(*view, Priority::Interactive)
            .await
            .unwrap();
    }
    let runner = {
        let indexer = indexer.clone();
        tokio::spawn(async move { indexer.run_until_idle_with(2).await })
    };
    let mut activated: BTreeMap<ViewId, Duration> = BTreeMap::new();
    let mut embedded: BTreeMap<ViewId, Duration> = BTreeMap::new();
    while embedded.len() < views.len() {
        let event = match tokio::time::timeout(Duration::from_secs(120), events.recv()).await {
            Ok(Ok(event)) => event,
            Ok(Err(RecvError::Lagged(_))) => continue,
            Ok(Err(RecvError::Closed)) | Err(_) => panic!("the build did not finish"),
        };
        match event.kind {
            ProgressKind::Activated { .. } => {
                activated.entry(event.view).or_insert(started.elapsed());
            }
            ProgressKind::Tier {
                tier: Tier::T2,
                state,
            } if state.is_final() => {
                assert_eq!(state, TierState::Done, "{}", event.project);
                embedded.entry(event.view).or_insert(started.elapsed());
            }
            _ => {}
        }
    }
    runner.await.unwrap().unwrap();
    let searchable = activated.values().max().copied().unwrap();
    let complete = embedded.values().max().copied().unwrap();
    let stats = indexer.stats();
    eprintln!(
        "Small fixture ({} views, {} files): all views searchable (T0+T1+T3, activated) after {searchable:?}; \
         embeddings complete (T2) after {complete:?} ({} inputs in {} provider calls of 40 ms)",
        views.len(),
        stats.files_read,
        stats.inputs_embedded,
        stats.embedding_calls
    );
    assert_eq!(activated.len(), views.len());
    for (view, at) in &activated {
        assert!(
            *at <= embedded[view],
            "a view's T2 finished before it was activated"
        );
    }
    assert!(searchable < complete);
    drop(indexer);
}
