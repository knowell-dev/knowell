//! The default T3 stage links contracts across the workspace's projects.

use std::sync::Arc;

use knowell_index::{Priority, TierState};

use crate::common::{
    CountingEmbedder, active_files, fixture_workspace, git_available, indexer, require_db, view_of,
};

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn contracts_are_linked_across_projects_and_unchanged_files_write_nothing() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(None);
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    for view in &registration.views {
        let status = indexer.status(view.view).await.unwrap();
        assert_eq!(status.tiers.t3, TierState::Done, "{}", view.project);
    }
    // Link rows exist and carry their stage origin.
    assert!(
        db.count("SELECT count(*) FROM contract WHERE origin LIKE 'link:%'")
            .await
            > 10
    );
    assert!(
        db.count(
            "SELECT count(*) FROM edge WHERE origin LIKE 'link:%' AND kind IN ('exposes', 'consumes')"
        )
        .await
            > 0
    );
    assert_eq!(
        db.count("SELECT count(*) FROM edge WHERE origin LIKE 'link:%' AND to_kind <> 'contract'")
            .await,
        0
    );
    // Some contract has a producer in one project and a consumer in
    // another: the cross-project link.
    let linked = db
        .count(
            "SELECT count(*) FROM (
               SELECT c.kind, c.key FROM contract c JOIN view v ON v.id = c.view_id
               GROUP BY c.kind, c.key
               HAVING count(DISTINCT v.project_id) FILTER (WHERE c.role = 'producer') > 0
                  AND count(DISTINCT v.project_id) FILTER (WHERE c.role = 'consumer') > 0
                  AND count(DISTINCT v.project_id) > 1
             ) linked",
        )
        .await;
    assert!(linked > 0, "no contract links two projects");
    // Link sources are mapped to indexed symbols where possible.
    assert!(
        db.count(
            "SELECT count(*) FROM contract WHERE origin LIKE 'link:%' AND symbol_id IS NOT NULL"
        )
        .await
            > 0
    );

    // The first rebuild of a project relinks it against every project that
    // is active by now (during the initial index some were still building),
    // which may update rows. After that, a change that touches no contract
    // rewrites no link rows: only origins whose rows differ are replaced.
    let project = "billing-api";
    let view = view_of(&registration, project);
    let mut written = Vec::new();
    for round in 0..2 {
        ws.write(
            project,
            &format!("docs/knowell-link-probe-{round}.md"),
            "# Notes\n\nnothing to link\n",
        );
        ws.commit_all(project, "a note");
        let before = indexer.stats().relation_rows_written;
        indexer
            .refresh_view(view, Priority::Interactive)
            .await
            .unwrap();
        indexer.run_until_idle().await.unwrap();
        let status = indexer.status(view).await.unwrap();
        assert_eq!(status.tiers.t3, TierState::Done);
        written.push(indexer.stats().relation_rows_written - before);
    }
    assert_eq!(written[1], 0, "rows rewritten per round: {written:?}");
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_failed_link_drops_the_rows_of_changed_files_and_still_activates() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let ws = fixture_workspace(Some(&["contracts"]));
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let (registration, _) = indexer(&db, data.path(), &embedder)
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, "contracts");
    let files = active_files(&db, view).await.len();
    // A restarted engine whose link stage allows one more file.
    let stage = knowell_index::LinkRelationStage::builtin()
        .unwrap()
        .with_max_files(files + 1);
    let indexer =
        knowell_index::Indexer::builder(db.store.clone(), crate::common::config(data.path()))
            .engine(&crate::common::engine())
            .embedder(crate::common::name("local"), Arc::clone(&embedder))
            .link_stage(Arc::new(stage))
            .build()
            .unwrap();
    indexer.register(&ws.resolved).await.unwrap();
    assert_eq!(
        indexer.status(view).await.unwrap().tiers.t3,
        TierState::Done
    );
    let rows = |sql: String| {
        let db = &db;
        async move { db.count(&sql).await }
    };
    let origin: String = {
        let mut conn = db.conn().await;
        sqlx::query_scalar(
            "SELECT origin FROM contract WHERE view_id = $1 AND valid_to IS NULL ORDER BY origin LIMIT 1",
        )
        .bind(view)
        .fetch_one(&mut *conn)
        .await
        .unwrap()
    };
    let changed = origin.strip_prefix("link:").unwrap().to_owned();
    let unchanged_rows = rows(format!(
        "SELECT count(*) FROM contract WHERE view_id = '{view}' AND valid_to IS NULL AND origin <> '{origin}'"
    ))
    .await;

    // The project grows past the link bound while a contract file changes.
    let mut text = ws.read("contracts", &changed);
    text.push('\n');
    ws.write("contracts", &changed, &text);
    ws.write("contracts", "notes/extra-1.md", "# Extra\n");
    ws.write("contracts", "notes/extra-2.md", "# Extra\n");
    ws.commit_all("contracts", "grow");
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    indexer.run_until_idle().await.unwrap();
    let status = indexer.status(view).await.unwrap();
    assert_eq!(
        status.active_commit.as_deref(),
        Some(ws.head("contracts").as_str()),
        "the generation is still activated"
    );
    let TierState::Failed { reason } = &status.tiers.t3 else {
        panic!("{:?}", status.tiers);
    };
    assert!(reason.contains("link stage's bound"), "{reason}");
    // The changed file's link rows are gone, the others stay.
    assert_eq!(
        rows(format!(
            "SELECT count(*) FROM contract WHERE view_id = '{view}' AND valid_to IS NULL AND origin = '{origin}'"
        ))
        .await,
        0
    );
    assert_eq!(
        rows(format!(
            "SELECT count(*) FROM contract WHERE view_id = '{view}' AND valid_to IS NULL AND origin <> '{origin}'"
        ))
        .await,
        unchanged_rows
    );
    drop(indexer);
}
