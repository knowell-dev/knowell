//! Incremental builds: modify, rename, delete, rewritten history.

use std::collections::BTreeMap;
use std::sync::Arc;

use knowell_config::{Origin, Sourced};
use knowell_core::ContentHash;
use knowell_index::{
    Priority, SyncOutcome, TierSkip, TierState, parser_version_tag, split_symbol_key,
};
use knowell_store::content;
use knowell_store::jobs;
use knowell_store::symbols;
use knowell_store::views::GenerationPin;
use knowell_store::{OccurrenceRole, SymbolId};
use tokio::sync::mpsc;

use crate::common::{
    CountingEmbedder, Workspace, active_files, active_pin, config, embed_with, fixture_workspace,
    git_available, indexer, indexer_with, path, require_db, view_of,
};

const PROJECT: &str = "billing-api";

/// The indexed file with the most chunks (ties by path), and its chunk count.
async fn biggest_file(
    db: &crate::common::TestDb,
    view: knowell_store::ViewId,
    org: knowell_store::OrganizationId,
) -> (knowell_core::RepoPath, usize) {
    let files = active_files(db, view).await;
    let mut conn = db.conn().await;
    let mut best = None;
    for (file, hash) in files {
        if file.extension() != Some("ts") {
            continue;
        }
        let n = content::chunks_of(&mut conn, org, &hash, &parser_version_tag())
            .await
            .unwrap()
            .len();
        if best.as_ref().is_none_or(|(_, m)| n > *m) {
            best = Some((file, n));
        }
    }
    best.unwrap()
}

/// Definitions (in-file name → symbol id) of `file` in a pinned generation.
async fn definitions(
    db: &crate::common::TestDb,
    pin: GenerationPin,
    file: &knowell_core::RepoPath,
) -> BTreeMap<String, SymbolId> {
    let mut conn = db.conn().await;
    let mut out = BTreeMap::new();
    for o in symbols::occurrences_in_file(&mut conn, pin, file)
        .await
        .unwrap()
    {
        if o.occurrence.role != OccurrenceRole::Definition {
            continue;
        }
        let symbol = symbols::get_symbol(&mut conn, o.occurrence.symbol)
            .await
            .unwrap()
            .unwrap();
        let (_, local) = split_symbol_key(&symbol.qualified_name).unwrap();
        out.insert(format!("{}:{local}", symbol.kind), symbol.id);
    }
    out
}

async fn refresh(indexer: &knowell_index::Indexer<CountingEmbedder>, view: knowell_store::ViewId) {
    let outcome = indexer
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    assert!(matches!(outcome, SyncOutcome::Queued { .. }), "{outcome:?}");
    indexer.run_until_idle().await.unwrap();
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.building_generation, None);
    assert_eq!(status.tiers.t2, TierState::Done, "{:?}", status.tiers);
}

async fn setup(ws: &Workspace) -> Option<()> {
    let _ = ws;
    git_available().then_some(())
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn modifying_one_file_reparses_and_reembeds_only_that_file() {
    let db = require_db!();
    let mut ws = fixture_workspace(Some(&[PROJECT]));
    if setup(&ws).await.is_none() {
        return;
    }
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let before_files = active_files(&db, view).await;
    let (file, chunks) = biggest_file(&db, view, registration.organization).await;
    assert!(chunks >= 3, "{file} has only {chunks} chunks");

    let stats = indexer.stats();
    let calls = embedder.calls();
    let inputs = embedder.inputs();
    let mut text = ws.read(PROJECT, file.as_str());
    text.push_str("\nexport function knowellProbeAddition(): number {\n  return 42;\n}\n");
    ws.write(PROJECT, file.as_str(), &text);
    let commit = ws.commit_all(PROJECT, "add a probe function");
    refresh(&indexer, view).await;

    let after = indexer.stats();
    assert_eq!(after.plans_incremental - stats.plans_incremental, 1);
    assert_eq!(
        after.files_read - stats.files_read,
        1,
        "only the changed blob is read"
    );
    assert_eq!(
        after.files_parsed - stats.files_parsed,
        1,
        "only the changed file is parsed"
    );
    let embedded = embedder.inputs() - inputs;
    assert!(embedded >= 1, "the changed chunk is embedded");
    assert!(
        embedded < chunks as u64,
        "{embedded} inputs re-embedded for a file of {chunks} chunks"
    );
    assert!(embedder.calls() > calls);
    assert_eq!(after.inputs_embedded - stats.inputs_embedded, embedded);

    // Only that path got a new version.
    let after_files = active_files(&db, view).await;
    let changed: Vec<_> = after_files
        .iter()
        .filter(|(p, h)| before_files.get(*p) != Some(*h))
        .map(|(p, _)| p.clone())
        .collect();
    assert_eq!(changed, vec![file.clone()]);
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.active_commit.as_deref(), Some(commit.as_str()));
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    let hits = lexical.search("knowellProbeAddition", 5).unwrap();
    assert_eq!(hits.first().map(|h| h.path.as_str()), Some(file.as_str()));
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rename_keeps_symbol_identity_and_delete_removes() {
    let db = require_db!();
    let mut ws = fixture_workspace(Some(&[PROJECT]));
    if setup(&ws).await.is_none() {
        return;
    }
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let (tx, mut stale_rx) = mpsc::unbounded_channel();
    let indexer = knowell_index::Indexer::builder(db.store.clone(), config(data.path()))
        .engine(&crate::common::engine())
        .embedder(crate::common::name("local"), Arc::clone(&embedder))
        .staleness_sender(tx)
        .build()
        .unwrap();
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let (file, _) = biggest_file(&db, view, registration.organization).await;
    let pin_before = active_pin(&db, view).await;
    let before = definitions(&db, pin_before, &file).await;
    assert!(before.len() >= 2, "{before:?}");
    let old_hash = active_files(&db, view).await[&file];

    // Rename without changing content.
    let renamed = path(&format!("moved/{}", file.file_name()));
    std::fs::create_dir_all(ws.project_dir(PROJECT).join("moved")).unwrap();
    ws.git(PROJECT, &["mv", file.as_str(), renamed.as_str()]);
    ws.commit_all(PROJECT, "move a file");
    let stats = indexer.stats();
    refresh(&indexer, view).await;
    assert!(indexer.stats().symbols_renamed > stats.symbols_renamed);

    let pin_after = active_pin(&db, view).await;
    assert!(pin_after.generation > pin_before.generation);
    let after = definitions(&db, pin_after, &renamed).await;
    assert_eq!(after, before, "same symbols, same ids");
    let mut conn = db.conn().await;
    for id in after.values() {
        let symbol = symbols::get_symbol(&mut conn, *id).await.unwrap().unwrap();
        assert!(
            symbol.qualified_name.starts_with(&format!("{renamed}#")),
            "{}",
            symbol.qualified_name
        );
    }
    let files = active_files(&db, view).await;
    assert!(!files.contains_key(&file));
    assert_eq!(files.get(&renamed), Some(&old_hash));
    let history = content::file_history(&mut conn, view, &renamed, 5)
        .await
        .unwrap();
    assert_eq!(history[0].renamed_from.as_ref(), Some(&file));
    assert_eq!(history.len(), 2, "history follows the rename");
    assert!(
        symbols::occurrences_in_file(&mut conn, pin_after, &file)
            .await
            .unwrap()
            .is_empty()
    );
    drop(conn);
    let event = stale_rx.try_recv().unwrap();
    assert_eq!(event.generation, pin_after.generation);
    assert_eq!(event.files.len(), 1);
    assert_eq!(event.files[0].path, file);
    assert_eq!(event.files[0].old_hash, old_hash);
    assert_eq!(event.files[0].renamed_to.as_ref(), Some(&renamed));

    // Delete it.
    let marker = after.keys().next().unwrap().clone();
    ws.git(PROJECT, &["rm", "-q", renamed.as_str()]);
    ws.commit_all(PROJECT, "delete the file");
    refresh(&indexer, view).await;
    let pin_deleted = active_pin(&db, view).await;
    assert!(!active_files(&db, view).await.contains_key(&renamed));
    let mut conn = db.conn().await;
    assert!(
        symbols::occurrences_in_file(&mut conn, pin_deleted, &renamed)
            .await
            .unwrap()
            .is_empty()
    );
    // The older generation still shows the file until it is pruned.
    assert!(
        !symbols::occurrences_in_file(&mut conn, pin_after, &renamed)
            .await
            .unwrap()
            .is_empty()
    );
    drop(conn);
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    let hits = lexical.search(&marker, 20).unwrap();
    assert!(hits.iter().all(|h| h.path != renamed.as_str()));
    let event = stale_rx.try_recv().unwrap();
    assert_eq!(event.files[0].path, renamed);
    assert_eq!(event.files[0].new_hash, None);
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn rewritten_history_reuses_content_and_embeddings() {
    let db = require_db!();
    let mut ws = fixture_workspace(Some(&[PROJECT]));
    if setup(&ws).await.is_none() {
        return;
    }
    embed_with(&mut ws.resolved, "local");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer_with(&db, config(data.path()), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let (file, chunks) = biggest_file(&db, view, registration.organization).await;
    let old_commit = ws.head(PROJECT);

    // Amend the only commit: the new commit is not a descendant of the
    // indexed one (what a force-push or rebase looks like).
    let mut text = ws.read(PROJECT, file.as_str());
    text.push_str("\nexport const knowellRewriteProbe = 'rewritten';\n");
    ws.write(PROJECT, file.as_str(), &text);
    ws.git(PROJECT, &["add", "-A"]);
    ws.git(PROJECT, &["commit", "-q", "--amend", "-m", "rewritten"]);
    let new_commit = ws.head(PROJECT);
    assert_ne!(old_commit, new_commit);

    let stats = indexer.stats();
    let inputs = embedder.inputs();
    let vectors = db.count("SELECT count(*) FROM embedding").await;
    refresh(&indexer, view).await;
    let after = indexer.stats();
    assert_eq!(after.plans_rewrite - stats.plans_rewrite, 1, "full re-walk");
    assert_eq!(
        after.files_read - stats.files_read,
        1,
        "known blobs are not read again"
    );
    assert_eq!(after.files_parsed - stats.files_parsed, 1);
    let embedded = embedder.inputs() - inputs;
    assert!(
        embedded >= 1 && embedded < chunks as u64,
        "{embedded} of {chunks}"
    );
    let new_vectors = db.count("SELECT count(*) FROM embedding").await;
    assert_eq!(u64::try_from(new_vectors - vectors).unwrap(), embedded);
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.active_commit.as_deref(), Some(new_commit.as_str()));

    // A forced rebuild of the same commit reads everything again but still
    // embeds nothing new.
    let calls = embedder.calls();
    let outcome = indexer
        .rebuild_view(view, Priority::Background)
        .await
        .unwrap();
    assert!(matches!(outcome, SyncOutcome::Queued { .. }), "{outcome:?}");
    indexer.run_until_idle().await.unwrap();
    assert_eq!(embedder.calls(), calls);
    assert_eq!(
        indexer.status(view).await.unwrap().active_commit.as_deref(),
        Some(new_commit.as_str())
    );
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn remote_branch_views_read_git_objects_and_leave_the_checkout_alone() {
    let db = require_db!();
    let mut ws = fixture_workspace(Some(&["contracts"]));
    if setup(&ws).await.is_none() {
        return;
    }
    embed_with(&mut ws.resolved, "local");
    // A clone whose checkout is on another branch, with local commits and
    // unsaved edits; the view follows `origin/main` only.
    let output = ws
        .command(&ws.root())
        .args(["clone", "-q", "contracts", "mirror"])
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    ws.git("mirror", &["checkout", "-q", "-b", "local-work"]);
    ws.write(
        "mirror",
        "notes/local.md",
        "# Local\n\nknowellLocalCommitProbe\n",
    );
    ws.commit_all("mirror", "local work");
    let readme = ws.manifest.projects[0].files[0].path.clone();
    let dirty = format!(
        "{}\nknowellDirtyProbe\n",
        ws.read("mirror", readme.as_str())
    );
    ws.write("mirror", readme.as_str(), &dirty);
    let local_head = ws.head("mirror");

    let mut mirror = ws.resolved.projects[0].clone();
    mirror.name = crate::common::name("mirror");
    mirror.path = ws.project_dir("mirror");
    mirror.track.value = "remote:origin/main".parse().unwrap();
    ws.resolved.projects = vec![mirror];
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, "mirror");
    let origin_head = ws.head("contracts");
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.active_commit.as_deref(), Some(origin_head.as_str()));
    let files = active_files(&db, view).await;
    assert!(!files.contains_key(&path("notes/local.md")));
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    assert!(lexical.search("knowellDirtyProbe", 5).unwrap().is_empty());
    assert!(
        lexical
            .search("knowellLocalCommitProbe", 5)
            .unwrap()
            .is_empty()
    );
    drop(lexical);

    // Upstream moves; after a fetch the view follows it.
    ws.write(
        "contracts",
        "notes/upstream.md",
        "# Upstream\n\nknowellUpstreamProbe\n",
    );
    let upstream = ws.commit_all("contracts", "upstream change");
    ws.git("mirror", &["fetch", "-q", "origin"]);
    refresh(&indexer, view).await;
    let status = indexer.status(view).await.unwrap();
    assert_eq!(status.active_commit.as_deref(), Some(upstream.as_str()));
    let lexical = indexer.lexical(view).await.unwrap().unwrap();
    assert_eq!(lexical.search("knowellUpstreamProbe", 5).unwrap().len(), 1);
    drop(lexical);

    // The checkout is exactly as the user left it.
    assert_eq!(ws.head("mirror"), local_head);
    assert_eq!(
        ws.git("mirror", &["branch", "--show-current"]),
        "local-work"
    );
    assert_eq!(ws.read("mirror", readme.as_str()), dirty);
    assert!(!ws.project_dir("mirror").join("notes/upstream.md").exists());
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unchanged_commit_with_changed_exclusions_reconciles_the_active_generation() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&[PROJECT]));
    embed_with(&mut ws.resolved, "local");
    let removed = path("policy/removed.ts");
    let retained = path("policy/retained.ts");
    ws.write(
        PROJECT,
        removed.as_str(),
        "export const qzxvmbnptlkrs = 1;\n",
    );
    ws.write(
        PROJECT,
        retained.as_str(),
        "export const qzplmnbvtsrjk = 2;\n",
    );
    let commit = ws.commit_all(PROJECT, "add synthetic policy probes");
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let original = indexer(&db, data.path(), &embedder);
    let (registration, _) = original
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let before = active_pin(&db, view).await;
    let mut expected_files = active_files(&db, view).await;
    assert!(expected_files.remove(&removed).is_some());
    assert!(expected_files.contains_key(&retained));
    let lexical = original.lexical(view).await.unwrap().unwrap();
    let removed_hits = lexical.search("qzxvmbnptlkrs", 5).unwrap();
    assert_eq!(removed_hits.len(), 1);
    assert_eq!(
        removed_hits.first().map(|hit| hit.path.as_str()),
        Some(removed.as_str())
    );
    drop(lexical);
    drop(original);

    // A fresh process sees a new policy but the exact same Git commit.
    ws.resolved
        .projects
        .iter_mut()
        .find(|project| project.name.as_str() == PROJECT)
        .unwrap()
        .exclude
        .push(Sourced {
            value: removed.as_str().to_owned(),
            origin: Origin::Workspace,
        });
    let changed = indexer(&db, data.path(), &embedder);
    let registration = changed.register(&ws.resolved).await.unwrap();
    assert_eq!(view_of(&registration, PROJECT), view);
    let sync = changed
        .refresh_view(view, Priority::Interactive)
        .await
        .unwrap();
    assert!(matches!(sync, SyncOutcome::Queued { created: true, .. }));
    let run = changed.run_until_idle().await.unwrap();
    assert!(run.jobs > 0);
    assert_eq!(run.failed, 0);
    assert_eq!(changed.stats().plans_full, 1);
    let after = active_pin(&db, view).await;
    assert_eq!(after.generation, before.generation + 1);
    assert_eq!(active_files(&db, view).await, expected_files);
    assert_eq!(
        changed.status(view).await.unwrap().active_commit.as_deref(),
        Some(commit.as_str())
    );
    assert_eq!(ws.head(PROJECT), commit);
    let lexical = changed.lexical(view).await.unwrap().unwrap();
    assert!(lexical.search("qzxvmbnptlkrs", 5).unwrap().is_empty());
    let retained_hits = lexical.search("qzplmnbvtsrjk", 5).unwrap();
    assert_eq!(retained_hits.len(), 1);
    assert_eq!(
        retained_hits.first().map(|hit| hit.path.as_str()),
        Some(retained.as_str())
    );
    drop(lexical);

    assert!(matches!(
        changed
            .refresh_view(view, Priority::Interactive)
            .await
            .unwrap(),
        SyncOutcome::UpToDate { .. }
    ));
    assert_eq!(changed.run_until_idle().await.unwrap().jobs, 0);
    assert_eq!(active_pin(&db, view).await, after);
    drop(changed);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn unchanged_commit_with_missing_or_corrupt_policy_manifest_rebuilds_conservatively() {
    let db = require_db!();
    if !git_available() {
        return;
    }
    let mut ws = fixture_workspace(Some(&[PROJECT]));
    embed_with(&mut ws.resolved, "local");
    let commit = ws.head(PROJECT);
    let data = tempfile::tempdir().unwrap();
    let embedder = Arc::new(CountingEmbedder::new());
    let indexer = indexer(&db, data.path(), &embedder);
    let (registration, _) = indexer
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    let view = view_of(&registration, PROJECT);
    let files = active_files(&db, view).await;
    assert!(!files.is_empty());
    let calls = embedder.calls();

    for corrupt in [false, true] {
        let before = active_pin(&db, view).await;
        let manifest = data
            .path()
            .join("views")
            .join(view.to_string())
            .join(format!("manifest-{}.json", before.generation));
        if corrupt {
            std::fs::write(&manifest, b"{invalid manifest").unwrap();
        } else {
            std::fs::remove_file(&manifest).unwrap();
        }
        let stats = indexer.stats();
        assert!(matches!(
            indexer
                .refresh_view(view, Priority::Interactive)
                .await
                .unwrap(),
            SyncOutcome::Queued { created: true, .. }
        ));
        let run = indexer.run_until_idle().await.unwrap();
        assert!(run.jobs > 0);
        assert_eq!(run.failed, 0);
        assert_eq!(indexer.stats().plans_full - stats.plans_full, 1);
        assert!(indexer.stats().files_read > stats.files_read);
        let after = active_pin(&db, view).await;
        assert_eq!(after.generation, before.generation + 1);
        assert_eq!(active_files(&db, view).await, files);
        assert_eq!(
            indexer.status(view).await.unwrap().active_commit.as_deref(),
            Some(commit.as_str())
        );
        assert_eq!(ws.head(PROJECT), commit);
        assert_eq!(embedder.calls(), calls, "stored embeddings are reused");
        assert!(matches!(
            indexer
                .refresh_view(view, Priority::Interactive)
                .await
                .unwrap(),
            SyncOutcome::UpToDate { .. }
        ));
        assert_eq!(indexer.run_until_idle().await.unwrap().jobs, 0);
        assert_eq!(active_pin(&db, view).await, after);
    }
    drop(indexer);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn advanced_commit_with_missing_or_corrupt_policy_manifest_rechecks_unchanged_file_sizes() {
    if !git_available() {
        return;
    }
    for (corrupt, disappear_after_queue) in
        [(false, false), (true, false), (false, true), (true, true)]
    {
        let db = require_db!();
        let mut ws = fixture_workspace(Some(&[PROJECT]));
        for project in &mut ws.resolved.projects {
            project.embedding.provider = None;
        }
        let oversized = path("policy/oversized.ts");
        let retained = path("policy/retained.ts");
        let advanced = path("policy/advanced.ts");
        let oversized_text = format!(
            "export const qzxvmbnptlkrs = 1;\n{}",
            "// synthetic size-limit padding\n".repeat(20)
        );
        let retained_text = "export const qzplmnbvtsrjk = 2;\n";
        ws.write(PROJECT, oversized.as_str(), &oversized_text);
        ws.write(PROJECT, retained.as_str(), retained_text);
        ws.write(
            PROJECT,
            advanced.as_str(),
            "export const qzvptmbkrlsn = 1;\n",
        );
        let old_commit = ws.commit_all(PROJECT, "add synthetic size policy probes");
        let data = tempfile::tempdir().unwrap();
        let embedder = Arc::new(CountingEmbedder::new());
        let original = indexer(&db, data.path(), &embedder);
        let (registration, _) = original
            .index_workspace(&ws.resolved, Priority::Interactive)
            .await
            .unwrap();
        let view = view_of(&registration, PROJECT);
        let before = active_pin(&db, view).await;
        let before_files = active_files(&db, view).await;
        assert_eq!(
            before_files.get(&oversized),
            Some(&ContentHash::of(oversized_text.as_bytes()))
        );
        assert_eq!(
            before_files.get(&retained),
            Some(&ContentHash::of(retained_text.as_bytes()))
        );
        let lexical = original.lexical(view).await.unwrap().unwrap();
        let hits = lexical.search("qzxvmbnptlkrs", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits.first().map(|hit| hit.path.as_str()),
            Some(oversized.as_str())
        );
        drop(lexical);

        let advanced_text = "export const qzvptmbkrlsn = 3;\n";
        ws.write(PROJECT, advanced.as_str(), advanced_text);
        let commit = ws.commit_all(PROJECT, "advance only the eligible policy probe");
        assert_ne!(commit, old_commit);
        // Exercise a queued unforced job whose trusted manifest later disappears,
        // as well as a refresh that starts with an unavailable manifest.
        let queued = if disappear_after_queue {
            Some(
                original
                    .refresh_view(view, Priority::Interactive)
                    .await
                    .unwrap(),
            )
        } else {
            None
        };
        drop(original);
        let manifest = data
            .path()
            .join("views")
            .join(view.to_string())
            .join(format!("manifest-{}.json", before.generation));
        if corrupt {
            std::fs::write(&manifest, b"{invalid manifest").unwrap();
        } else {
            std::fs::remove_file(&manifest).unwrap();
        }
        let mut settings = config(data.path());
        settings.limits.max_file_bytes = 128;
        assert!(oversized_text.len() > 128);
        assert!(retained_text.len() < 128 && advanced_text.len() < 128);
        let changed = indexer_with(&db, settings, &embedder);
        let registration = changed.register(&ws.resolved).await.unwrap();
        assert_eq!(view_of(&registration, PROJECT), view);
        let sync = match queued {
            Some(queued) => queued,
            None => changed
                .refresh_view(view, Priority::Interactive)
                .await
                .unwrap(),
        };
        let SyncOutcome::Queued {
            target,
            job,
            created: true,
            ..
        } = sync
        else {
            panic!("the advanced synthetic target must queue a build");
        };
        assert_eq!(target.commit(), Some(commit.as_str()));
        let mut conn = db.conn().await;
        let queued = jobs::get_job(&mut conn, job).await.unwrap().unwrap();
        assert_eq!(queued.payload["force"], !disappear_after_queue);
        drop(conn);
        let run = changed.run_until_idle().await.unwrap();
        assert!(run.jobs > 0);
        assert_eq!(run.failed, 0);
        assert_eq!(changed.stats().plans_full, 1);
        assert_eq!(changed.stats().plans_incremental, 0);
        let after = active_pin(&db, view).await;
        assert_eq!(after.generation, before.generation + 1);
        let after_files = active_files(&db, view).await;
        assert!(!after_files.contains_key(&oversized));
        assert_eq!(after_files.get(&retained), before_files.get(&retained));
        assert_eq!(
            after_files.get(&advanced),
            Some(&ContentHash::of(advanced_text.as_bytes()))
        );
        let status = changed.status(view).await.unwrap();
        assert_eq!(status.active_commit.as_deref(), Some(commit.as_str()));
        assert_eq!(status.latest_seen_commit.as_deref(), Some(commit.as_str()));
        assert_eq!(status.tiers.t0, TierState::Done);
        assert_eq!(status.tiers.t1, TierState::Done);
        assert_eq!(status.tiers.t3, TierState::Done);
        assert_eq!(
            status.tiers.t2,
            TierState::Skipped {
                reason: TierSkip::NoProvider
            }
        );
        assert_eq!(embedder.calls(), 0);
        assert_eq!(ws.head(PROJECT), commit);
        assert_eq!(ws.read(PROJECT, oversized.as_str()), oversized_text);
        assert_eq!(ws.read(PROJECT, retained.as_str()), retained_text);
        let lexical = changed.lexical(view).await.unwrap().unwrap();
        assert!(lexical.search("qzxvmbnptlkrs", 5).unwrap().is_empty());
        let hits = lexical.search("qzplmnbvtsrjk", 5).unwrap();
        assert_eq!(hits.len(), 1);
        assert_eq!(
            hits.first().map(|hit| hit.path.as_str()),
            Some(retained.as_str())
        );
        drop(lexical);
        assert!(matches!(
            changed
                .refresh_view(view, Priority::Interactive)
                .await
                .unwrap(),
            SyncOutcome::UpToDate { .. }
        ));
        assert_eq!(changed.run_until_idle().await.unwrap().jobs, 0);
        assert_eq!(active_pin(&db, view).await, after);
        drop(changed);
    }
}
