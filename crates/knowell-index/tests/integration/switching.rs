//! Blue-green embedding profile switches: the serving profile keeps serving
//! while the target builds, the flip moves every view at once, switches
//! survive a restart, rollbacks reuse the old vectors, and a profile is only
//! ever produced by its own embedder.

use std::path::Path;
use std::sync::Arc;

use knowell_config::{EmbeddingPreset, Origin, ResolvedWorkspace, Sourced, parse_engine};
use knowell_embed::FAKE_MODEL;
use knowell_index::{EmbeddingPlan, IndexError, Indexer, Priority, Registration};
use knowell_store::embeddings::{self, NewEmbeddingProfile};
use knowell_store::switches::{self, SwitchOrigin};
use knowell_store::views::GenerationPin;
use knowell_store::{GenerationState, ProfileId, ProfileSwitchState, ViewId};

use crate::common::{
    CountingEmbedder, TestDb, active_pin, config, fixture_workspace, git_available, name,
    require_db, view_of,
};

const MAIN: &str = "contracts";
const OTHER: &str = "handbook";
const WIDE: u32 = 64;
const NARROW: u32 = 32;

/// Embedders `wide` (64 dimensions) and `narrow` (32), both local.
struct Embedders {
    wide: Arc<CountingEmbedder>,
    narrow: Arc<CountingEmbedder>,
}

impl Embedders {
    fn new() -> Self {
        Self {
            wide: Arc::new(CountingEmbedder::with_dims(WIDE)),
            narrow: Arc::new(CountingEmbedder::with_dims(NARROW)),
        }
    }

    fn calls(&self) -> u64 {
        self.wide.calls() + self.narrow.calls()
    }

    /// A fresh indexer (as a restarted process) with both embedders.
    fn indexer(&self, db: &TestDb, data: &Path) -> Indexer<CountingEmbedder> {
        let engine = parse_engine(&format!(
            "version = 1\n[providers.wide]\nkind = \"ollama\"\nmodel = \"{FAKE_MODEL}\"\n[providers.narrow]\nkind = \"ollama\"\nmodel = \"{FAKE_MODEL}\"\n"
        ))
        .unwrap();
        Indexer::builder(db.store.clone(), config(data))
            .engine(&engine)
            .embedder(name("wide"), Arc::clone(&self.wide))
            .embedder(name("narrow"), Arc::clone(&self.narrow))
            .build()
            .unwrap()
    }
}

/// Makes `project` embed with `provider` at `dims` dimensions.
fn embed(resolved: &mut ResolvedWorkspace, project: &str, provider: &str, dims: u32) {
    let project = resolved
        .projects
        .iter_mut()
        .find(|p| p.name.as_str() == project)
        .unwrap();
    project.embedding.provider = Some(Sourced {
        value: name(provider),
        origin: Origin::Project,
    });
    project.embedding.model = Some(Sourced {
        value: FAKE_MODEL.to_owned(),
        origin: Origin::Project,
    });
    project.embedding.preset = Sourced {
        value: EmbeddingPreset::Custom,
        origin: Origin::Project,
    };
    project.embedding.dimensions = Sourced {
        value: dims,
        origin: Origin::Project,
    };
}

fn planned(registration: &Registration, project: &str) -> ProfileId {
    let view = registration
        .views
        .iter()
        .find(|v| v.project.as_str() == project)
        .unwrap();
    match &view.embedding {
        EmbeddingPlan::Embed { profile, .. } => *profile,
        other => panic!("{project} does not embed: {other:?}"),
    }
}

async fn serving(db: &TestDb, view: ViewId) -> Option<ProfileId> {
    let mut conn = db.conn().await;
    switches::view_embeddings(&mut conn, &[view])
        .await
        .unwrap()
        .get(&view)
        .and_then(|row| row.serving)
}

/// Whether `profile` has active (complete) vectors for the active generation.
async fn covers(db: &TestDb, view: ViewId, profile: ProfileId) -> bool {
    let pin: GenerationPin = active_pin(db, view).await;
    let mut conn = db.conn().await;
    embeddings::index_generation_at(&mut conn, pin, profile)
        .await
        .unwrap()
        .is_some_and(|ig| ig.state == GenerationState::Active)
}

/// The workspace with `MAIN` on `wide` and `OTHER` on `narrow`, so both
/// profiles are registered.
fn workspace() -> (crate::common::Workspace, ResolvedWorkspace) {
    let ws = fixture_workspace(Some(&[MAIN, OTHER]));
    let mut resolved = ws.resolved.clone();
    embed(&mut resolved, MAIN, "wide", WIDE);
    embed(&mut resolved, OTHER, "narrow", NARROW);
    (ws, resolved)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_requested_switch_survives_a_restart_and_rolls_back_without_provider_calls() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let (_ws, resolved) = workspace();
    let data = tempfile::tempdir().unwrap();
    let embedders = Embedders::new();
    let first = embedders.indexer(&db, data.path());
    let (registration, _) = first
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    let main = view_of(&registration, MAIN);
    let other = view_of(&registration, OTHER);
    let wide = planned(&registration, MAIN);
    let narrow = planned(&registration, OTHER);
    assert_eq!(serving(&db, main).await, Some(wide));
    assert!(covers(&db, main, wide).await);

    let started = first
        .start_switch(&[main], narrow, "user:alice", 3600)
        .await
        .unwrap();
    assert_eq!(started.state, ProfileSwitchState::Building);
    assert_eq!((started.from, started.to), (Some(wide), narrow));
    // Queued, not run: the old profile still serves.
    let queued = db
        .count("SELECT count(*) FROM job WHERE idempotency_key LIKE '%:p%' AND state = 'queued'")
        .await;
    assert_eq!(queued, 1);
    assert_eq!(serving(&db, main).await, Some(wide));
    let progress = first.switch_progress(&started).await.unwrap();
    assert_eq!(progress.len(), 1);
    assert!(!progress[0].covered);
    assert_eq!(progress[0].embedded, 0);
    drop(first);

    // A fresh process resumes the switch from the store and flips it.
    let second = embedders.indexer(&db, data.path());
    let before = embedders.narrow.calls();
    second
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    assert!(embedders.narrow.calls() > before);
    assert!(covers(&db, main, narrow).await);
    assert_eq!(serving(&db, main).await, Some(narrow));
    assert_eq!(serving(&db, other).await, Some(narrow));
    let active = second.switch(started.id).await.unwrap().unwrap();
    assert_eq!(active.state, ProfileSwitchState::Active);
    assert!(
        second
            .switch_progress(&active)
            .await
            .unwrap()
            .iter()
            .all(|p| p.covered && p.embedded == p.inputs && p.inputs > 0)
    );
    // The old profile's vectors are kept.
    assert!(covers(&db, main, wide).await);

    // Rolling back before the next commit reuses them: no provider call.
    let calls = embedders.calls();
    let reverse = second
        .rollback_switch(started.id, "user:alice")
        .await
        .unwrap();
    assert_eq!(reverse.state, ProfileSwitchState::Active);
    assert_eq!(reverse.origin, SwitchOrigin::Rollback);
    assert_eq!(serving(&db, main).await, Some(wide));
    assert_eq!(embedders.calls(), calls);
    assert_eq!(
        second.switch(started.id).await.unwrap().unwrap().state,
        ProfileSwitchState::RolledBack
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn commits_during_a_switch_keep_the_serving_profile_current_and_outages_are_retried() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let (ws, resolved) = workspace();
    let data = tempfile::tempdir().unwrap();
    let embedders = Embedders::new();
    let indexer = embedders.indexer(&db, data.path());
    let (registration, _) = indexer
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    let main = view_of(&registration, MAIN);
    let wide = planned(&registration, MAIN);
    let narrow = planned(&registration, OTHER);
    let first_generation = active_pin(&db, main).await.generation;

    // The target's provider is down: its catch-up fails after the retries,
    // and the switch keeps building with the old profile serving.
    embedders.narrow.set_down(true);
    let started = indexer
        .start_switch(&[main], narrow, "user:alice", 3600)
        .await
        .unwrap();
    drain(&indexer, &db).await;
    assert_eq!(
        indexer.switch(started.id).await.unwrap().unwrap().state,
        ProfileSwitchState::Building
    );
    assert_eq!(serving(&db, main).await, Some(wide));

    // A commit while it builds: the new generation gets the serving
    // profile's vectors, so semantic search never has a gap.
    ws.write(
        MAIN,
        "switch-note.md",
        "# Switch note

A synthetic note written during a profile switch.
",
    );
    ws.commit_all(MAIN, "add a note during the switch");
    indexer
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    drain(&indexer, &db).await;
    assert!(active_pin(&db, main).await.generation > first_generation);
    assert!(covers(&db, main, wide).await);
    assert!(!covers(&db, main, narrow).await);
    let progress = indexer.switch_progress(&started).await.unwrap();
    assert!(!progress[0].covered);
    assert!(progress[0].failure.is_some());
    assert_eq!(serving(&db, main).await, Some(wide));
    // The periodic reconciliation does not retry a failed build by itself.
    let calls = embedders.narrow.calls();
    indexer.reconcile().await.unwrap();
    drain(&indexer, &db).await;
    assert_eq!(embedders.narrow.calls(), calls);

    // The provider is back; an explicit index run retries and flips.
    embedders.narrow.set_down(false);
    indexer
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    drain(&indexer, &db).await;
    assert!(covers(&db, main, narrow).await);
    assert_eq!(serving(&db, main).await, Some(narrow));
    assert_eq!(
        indexer.switch(started.id).await.unwrap().unwrap().state,
        ProfileSwitchState::Active
    );
}

/// Runs jobs until none is queued, running or waiting for a retry.
async fn drain(indexer: &Indexer<CountingEmbedder>, db: &TestDb) {
    for _ in 0..300 {
        indexer.run_until_idle().await.unwrap();
        let pending = db
            .count("SELECT count(*) FROM job WHERE state IN ('queued', 'running', 'failed')")
            .await;
        if pending == 0 {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
    }
    panic!("the queue did not drain");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn a_configuration_change_starts_one_switch_and_switching_back_reuses_vectors() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let (_ws, resolved) = workspace();
    let data = tempfile::tempdir().unwrap();
    let embedders = Embedders::new();
    let indexer = embedders.indexer(&db, data.path());
    let (registration, _) = indexer
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    let main = view_of(&registration, MAIN);
    let wide = planned(&registration, MAIN);
    let narrow = planned(&registration, OTHER);

    // Registering a changed configuration (as a search or status would)
    // schedules nothing and changes nothing.
    let mut changed = resolved.clone();
    embed(&mut changed, MAIN, "narrow", NARROW);
    indexer.register(&changed).await.unwrap();
    assert!(indexer.switches(10).await.unwrap().is_empty());
    assert_eq!(serving(&db, main).await, Some(wide));

    // Indexing it starts one configuration switch, which flips once built.
    indexer
        .index_workspace(&changed, Priority::Interactive)
        .await
        .unwrap();
    let switches = indexer.switches(10).await.unwrap();
    assert_eq!(switches.len(), 1);
    assert_eq!(switches[0].origin, SwitchOrigin::Configuration);
    assert_eq!(switches[0].requested_by, "configuration");
    assert_eq!(switches[0].state, ProfileSwitchState::Active);
    assert_eq!(serving(&db, main).await, Some(narrow));
    // The same configuration again starts nothing new.
    indexer
        .index_workspace(&changed, Priority::Interactive)
        .await
        .unwrap();
    assert_eq!(indexer.switches(10).await.unwrap().len(), 1);

    // Back to the first configuration: the old vectors still cover the
    // generation, so it flips without provider calls.
    let calls = embedders.calls();
    indexer
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    assert_eq!(indexer.switches(10).await.unwrap().len(), 2);
    assert_eq!(serving(&db, main).await, Some(wide));
    assert_eq!(embedders.calls(), calls);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn switches_never_substitute_another_embedder_for_a_profile() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let (_ws, resolved) = workspace();
    let data = tempfile::tempdir().unwrap();
    let embedders = Embedders::new();
    let indexer = embedders.indexer(&db, data.path());
    let (registration, _) = indexer
        .index_workspace(&resolved, Priority::Interactive)
        .await
        .unwrap();
    let main = view_of(&registration, MAIN);
    let other = view_of(&registration, OTHER);
    let wide = planned(&registration, MAIN);
    // A stored profile no configured embedder produces (another model).
    let mut conn = db.conn().await;
    let orphan = embeddings::register_profile(
        &mut conn,
        registration.organization,
        &NewEmbeddingProfile {
            name: name("synthetic-orphan-48"),
            provider: "ollama".to_owned(),
            model: "synthetic-unconfigured-model".to_owned(),
            dimensions: 48,
            input_format_version: "synthetic-format".to_owned(),
        },
    )
    .await
    .unwrap();
    drop(conn);
    let calls = embedders.calls();
    for (views, to) in [
        (vec![main], orphan.id),
        (vec![main], wide),
        (vec![main, other], wide),
        (Vec::new(), wide),
    ] {
        let err = indexer
            .start_switch(&views, to, "user:alice", 3600)
            .await
            .unwrap_err();
        assert!(matches!(err, IndexError::Invalid { .. }), "{err}");
    }
    assert!(indexer.switches(10).await.unwrap().is_empty());
    assert_eq!(embedders.calls(), calls);
    assert_eq!(serving(&db, main).await, Some(wide));
}
