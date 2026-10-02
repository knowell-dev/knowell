//! Initial indexing of the ten-project fixture.

use std::sync::Arc;
use std::time::Instant;

use knowell_index::{EmbeddingPlan, Priority, SyncOutcome, TierState};
use knowell_secrets::ExclusionPolicy;
use knowell_store::content;

use crate::common::{
    CountingEmbedder, active_files, active_pin, embed_with, fixture_workspace, git_available,
    indexer, require_db,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn initial_index_of_the_fixture() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(None);
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);

    let started = Instant::now();
    let (registration, outcomes) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let elapsed = started.elapsed();
    let stats = indexer.stats();
    eprintln!(
        "initial index of the Small fixture: {elapsed:?} ({} files read, {} parsed, {} chunks, {} inputs embedded in {} calls)",
        stats.files_read,
        stats.files_parsed,
        stats.chunks_written,
        stats.inputs_embedded,
        stats.embedding_calls
    );

    assert_eq!(registration.views.len(), 10, "{:?}", registration.issues);
    assert!(registration.issues.is_empty());
    assert!(
        registration
            .views
            .iter()
            .all(|v| matches!(v.embedding, EmbeddingPlan::Embed { .. }))
    );
    assert!(
        outcomes
            .iter()
            .all(|o| matches!(o, SyncOutcome::Queued { created: true, .. })),
        "{outcomes:?}"
    );

    let builtin = ExclusionPolicy::builtin();
    let mut total_files = 0usize;
    for view in &registration.views {
        let status = indexer.status(view.view).await.unwrap();
        let project = ws
            .manifest
            .projects
            .iter()
            .find(|p| p.name == view.project)
            .unwrap();
        assert_eq!(status.active_commit, project.commit, "{}", view.project);
        assert_eq!(status.latest_seen_commit, project.commit);
        assert_eq!(status.building_generation, None);
        assert_eq!(status.lag, None);
        assert_eq!(status.last_error, None);
        for tier in [
            &status.tiers.t0,
            &status.tiers.t1,
            &status.tiers.t2,
            &status.tiers.t3,
        ] {
            assert_eq!(
                tier,
                &TierState::Done,
                "{}: {:?}",
                view.project,
                status.tiers
            );
        }
        // Every file of the project is indexed except those excluded by path.
        let files = active_files(&db, view.view).await;
        let expected: Vec<_> = project
            .files
            .iter()
            .filter(|f| builtin.check(&f.path).is_none())
            .collect();
        assert_eq!(files.len(), expected.len(), "{}", view.project);
        for file in expected {
            assert_eq!(files.get(&file.path), Some(&file.hash), "{}", file.path);
        }
        total_files += files.len();
    }
    assert!(total_files >= 240, "{total_files} files indexed");

    // The committed `.env` of `infra` was never read, and no secret canary
    // reached stored content or the lexical index.
    let infra = registration
        .views
        .iter()
        .find(|v| v.project.as_str() == "infra")
        .unwrap();
    assert!(
        !active_files(&db, infra.view)
            .await
            .contains_key(&crate::common::path(".env"))
    );
    assert!(!ws.canaries.is_empty());
    let mut conn = db.conn().await;
    for view in &registration.views {
        let pin = active_pin(&db, view.view).await;
        let org = registration.organization;
        for file in content::files_at(&mut conn, pin).await.unwrap() {
            let stored = content::get_content(&mut conn, org, &file.content_hash)
                .await
                .unwrap()
                .unwrap();
            let text = stored.redacted_text.unwrap_or_default();
            for canary in &ws.canaries {
                assert!(!text.contains(canary.as_str()), "canary in {}", file.path);
            }
        }
        let lexical = indexer.lexical(view.view).await.unwrap().unwrap();
        for canary in &ws.canaries {
            assert!(lexical.search(canary, 5).unwrap().is_empty());
        }
    }
    drop(conn);

    // Rows exist for every layer.
    assert!(db.count("SELECT count(*) FROM symbol").await > 100);
    assert!(db.count("SELECT count(*) FROM chunk").await > 200);
    assert!(db.count("SELECT count(*) FROM occurrence").await > 100);
    assert!(
        db.count(
            "SELECT count(*) FROM edge WHERE kind = 'defines' AND evidence_type = 'syntactic'"
        )
        .await
            > 100
    );
    assert!(
        db.count("SELECT count(*) FROM edge WHERE kind = 'imports'")
            .await
            > 10
    );
    let vectors = db.count("SELECT count(*) FROM embedding").await;
    assert!(vectors > 100);
    assert_eq!(
        db.count("SELECT count(*) FROM index_generation WHERE state = 'active'")
            .await,
        10
    );
    assert_eq!(embedder.inputs(), stats.inputs_embedded);
    assert_eq!(u64::try_from(vectors).unwrap(), stats.inputs_embedded);

    // Lexical search works on the active generation.
    let billing = registration
        .views
        .iter()
        .find(|v| v.project.as_str() == "billing-api")
        .unwrap();
    let hits = indexer
        .lexical(billing.view)
        .await
        .unwrap()
        .unwrap()
        .search("cancel subscription", 5)
        .unwrap();
    assert!(!hits.is_empty());

    // Indexing again changes nothing and calls no provider.
    let calls = embedder.calls();
    let (_, again) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    assert!(
        again
            .iter()
            .all(|o| matches!(o, SyncOutcome::UpToDate { .. })),
        "{again:?}"
    );
    assert_eq!(embedder.calls(), calls);
    assert_eq!(
        db.count("SELECT count(*) FROM view_generation").await,
        10,
        "no new generations"
    );
    drop(indexer);
}
