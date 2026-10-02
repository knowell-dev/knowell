//! Prepared embedding inputs per path: identical content at two paths, and
//! a renamed file, each get their own inputs and vectors.

use std::collections::BTreeSet;
use std::sync::Arc;

use knowell_core::{ContentHash, RepoPath};
use knowell_index::{EmbeddingPlan, Priority, parser_version_tag};
use knowell_store::content;
use knowell_store::embeddings;
use knowell_store::views::GenerationPin;

use crate::common::{
    CountingEmbedder, TestDb, active_pin, embed_with, fixture_workspace, git_available, indexer,
    path, require_db, view_of,
};

const PROJECT: &str = "billing-api";

async fn inputs_of(db: &TestDb, pin: GenerationPin, file: &RepoPath) -> BTreeSet<ContentHash> {
    let mut conn = db.conn().await;
    content::chunk_inputs_at(
        &mut conn,
        pin,
        &parser_version_tag(),
        Some(std::slice::from_ref(file)),
    )
    .await
    .unwrap()
    .into_iter()
    .filter(|i| i.embed)
    .map(|i| i.prepared_input_hash)
    .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn identical_files_and_renamed_files_get_their_own_inputs() {
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
    let EmbeddingPlan::Embed { profile, .. } = registration.views[0].embedding.clone() else {
        panic!("the project embeds");
    };

    let text = "export function knowellDuplicateProbe(amount: number): number {\n  return amount * 2;\n}\n";
    ws.write(PROJECT, "src/dup/first.ts", text);
    ws.write(PROJECT, "src/dup/second.ts", text);
    ws.commit_all(PROJECT, "two identical files");
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    indexer.run_until_idle().await.unwrap();
    let pin = active_pin(&db, view).await;
    let (first, second) = (path("src/dup/first.ts"), path("src/dup/second.ts"));
    let first_inputs = inputs_of(&db, pin, &first).await;
    let second_inputs = inputs_of(&db, pin, &second).await;
    assert!(!first_inputs.is_empty());
    assert_eq!(first_inputs.len(), second_inputs.len());
    assert!(
        first_inputs.is_disjoint(&second_inputs),
        "the prepared input includes the path"
    );
    let mut conn = db.conn().await;
    let all: Vec<ContentHash> = first_inputs.iter().chain(&second_inputs).copied().collect();
    assert!(
        embeddings::missing_embeddings(&mut conn, profile, &all)
            .await
            .unwrap()
            .is_empty(),
        "both paths are embedded"
    );
    // A vector hit leads back to its own path only.
    let hit = *second_inputs.iter().next().unwrap();
    let located =
        content::locate_chunk_inputs(&mut conn, registration.organization, &[pin], &[hit])
            .await
            .unwrap();
    assert_eq!(
        located.iter().map(|l| l.path.clone()).collect::<Vec<_>>(),
        vec![second.clone()]
    );
    drop(conn);

    // Renaming a file changes its prepared input (it includes the path), so
    // the moved file is embedded again: the documented trade-off.
    let calls = embedder.inputs();
    let moved = path("src/dup/moved.ts");
    ws.git(PROJECT, &["mv", second.as_str(), moved.as_str()]);
    ws.commit_all(PROJECT, "move one copy");
    indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    indexer.run_until_idle().await.unwrap();
    let pin = active_pin(&db, view).await;
    let moved_inputs = inputs_of(&db, pin, &moved).await;
    assert_eq!(moved_inputs.len(), second_inputs.len());
    assert!(moved_inputs.is_disjoint(&second_inputs));
    assert_eq!(
        embedder.inputs() - calls,
        moved_inputs.len() as u64,
        "exactly the moved file's inputs were embedded"
    );
    let mut conn = db.conn().await;
    assert!(
        content::locate_chunk_inputs(&mut conn, registration.organization, &[pin], &[hit])
            .await
            .unwrap()
            .is_empty(),
        "the old path's input no longer locates anything in the new generation"
    );
    drop(conn);
    let coverage = indexer.embedding_coverage(view).await.unwrap().unwrap();
    assert!(coverage.complete && !coverage.is_partial(), "{coverage:?}");
    drop(indexer);
}
