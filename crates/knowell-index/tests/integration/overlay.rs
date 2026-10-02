//! Personal overlays never touch the shared store.

use std::sync::Arc;

use knowell_index::Priority;
use knowell_store::content;

use crate::common::{
    CountingEmbedder, active_files, active_pin, fixture_workspace, git_available, indexer, path,
    require_db, view_of,
};

const PROJECT: &str = "contracts";

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn worktree_changes_are_visible_only_through_the_overlay() {
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
    let files_before = active_files(&db, view).await;
    let pin_before = active_pin(&db, view).await;

    // Uncommitted: one modified, one added, one deleted file.
    let (modified, _) = files_before.iter().next().unwrap();
    let deleted = files_before.keys().nth(1).unwrap().clone();
    let modified = modified.clone();
    let mut text = ws.read(PROJECT, modified.as_str());
    text.push_str("\nknowellOverlayProbe modified locally\n");
    ws.write(PROJECT, modified.as_str(), &text);
    ws.write(
        PROJECT,
        "notes/overlay-probe.md",
        "# Draft\n\nknowellOverlayProbe drafted locally\n",
    );
    std::fs::remove_file(ws.project_dir(PROJECT).join(deleted.as_str())).unwrap();
    // A secret file in the worktree is excluded by path, never read.
    ws.write(PROJECT, ".env", "PASSWORD=knowellOverlayProbe\n");

    let overlay = indexer
        .build_overlay(view, &ws.project_dir(PROJECT))
        .await
        .unwrap();
    let added = path("notes/overlay-probe.md");
    let shadowed = overlay.shadowed_paths();
    assert!(shadowed.contains(&modified), "{shadowed:?}");
    assert!(shadowed.contains(&added));
    assert!(shadowed.contains(&deleted));
    assert!(!shadowed.contains(&path(".env")));
    assert_eq!(overlay.deleted().iter().collect::<Vec<_>>(), vec![&deleted]);
    assert_eq!(overlay.base_generation(), Some(pin_before.generation));
    let hits = overlay.search("knowellOverlayProbe", 10).unwrap();
    let mut paths: Vec<&str> = hits.iter().map(|h| h.path.as_str()).collect();
    paths.sort_unstable();
    let mut expected = vec![added.as_str(), modified.as_str()];
    expected.sort_unstable();
    assert_eq!(paths, expected);
    assert!(
        overlay
            .file(&added)
            .is_some_and(|f| f.text.contains("knowellOverlayProbe"))
    );
    assert!(Arc::ptr_eq(&indexer.overlay(view).unwrap(), &overlay));

    // The shared view did not change at all.
    assert_eq!(active_pin(&db, view).await, pin_before);
    assert_eq!(active_files(&db, view).await, files_before);
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    assert!(
        lexical
            .search("knowellOverlayProbe", 10)
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        db.count("SELECT count(*) FROM content WHERE redacted_text LIKE '%knowellOverlayProbe%'")
            .await,
        0
    );
    let mut conn = db.conn().await;
    assert!(
        content::file_at(&mut conn, pin_before, &added)
            .await
            .unwrap()
            .is_none()
    );
    drop(conn);

    // Reverting the worktree empties the overlay.
    ws.git(PROJECT, &["checkout", "--", "."]);
    std::fs::remove_file(ws.project_dir(PROJECT).join("notes/overlay-probe.md")).unwrap();
    std::fs::remove_file(ws.project_dir(PROJECT).join(".env")).unwrap();
    let overlay = indexer
        .build_overlay(view, &ws.project_dir(PROJECT))
        .await
        .unwrap();
    assert!(overlay.is_empty(), "{overlay:?}");
    drop(indexer);
}
