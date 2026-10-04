//! The in-process engine: shared state, the builder, and the public API for
//! standalone mode (`register`, `index_workspace`, `refresh_view`, `status`).

use std::collections::BTreeMap;
use std::fmt;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError, RwLock};
use std::time::Duration;

use knowell_config::{EngineConfig, ProviderConfig, ResolvedWorkspace};
use knowell_core::{ContentHash, Name, RepoPath};
use knowell_embed::{AnyEmbedder, Embedder};
use knowell_lexical::LexicalIndex;
use knowell_store::embeddings;
use knowell_store::jobs::{self, ClaimScope, Job, JobScope};
use knowell_store::views::{self, GenerationPin, View};
use knowell_store::{GenerationState, JobId, JobState, PgConnection, SourceKind, Store, ViewId};
use serde::Serialize;
use time::OffsetDateTime;
use tokio::sync::{Notify, broadcast, mpsc};
use tokio_util::sync::CancellationToken;

use crate::analyze::{AnalysedFile, parser_version_tag};
use crate::config::{IndexerConfig, Priority};
use crate::context::{EmbeddingDecision, Registration, ViewContext, register_workspace};
use crate::embeddings_stage::BUDGET_EXHAUSTED;
use crate::error::IndexError;
use crate::jobs::{BuildTarget, JOB_KINDS, StagePayload, stage_key};
use crate::lexical::LexicalStore;
use crate::link::LinkRelationStage;
use crate::overlay::Overlay;
use crate::pipeline::store_tree_hash;
use crate::relate::{NoRelations, RelationStage, StalenessEvent};
use crate::status::{
    EmbeddingCoverage, ProgressEvent, ProgressKind, Runtime, Tier, TierSkip, TierState, TierStates,
    ViewRuntime, ViewStatus,
};

/// Capacity of the progress broadcast channel.
const EVENT_CAPACITY: usize = 1024;

/// Counters of the work this indexer did since it was built. Tests and the
/// panel use them to show that incremental builds stay incremental.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct IndexStats {
    /// Jobs run to an end (succeeded, superseded or failed).
    pub jobs_run: u64,
    /// Files or blobs read and redacted from sources.
    pub files_read: u64,
    /// Files parsed (T1, and re-analysis in later stages).
    pub files_parsed: u64,
    /// Chunk rows written.
    pub chunks_written: u64,
    /// Symbols renamed to follow a moved file (identity kept).
    pub symbols_renamed: u64,
    /// Embedding calls made (each a batch of inputs).
    pub embedding_calls: u64,
    /// Prepared inputs sent to a provider.
    pub inputs_embedded: u64,
    /// Prepared inputs whose vector already existed (not sent again).
    pub inputs_reused: u64,
    /// Prepared inputs the provider refused as too long (not embedded).
    pub inputs_rejected: u64,
    /// Generations activated.
    pub generations_activated: u64,
    /// Builds that ended because a newer build replaced them.
    pub builds_superseded: u64,
    /// Builds planned from a diff of the active commit.
    pub plans_incremental: u64,
    /// Builds planned as a full re-walk after rewritten history.
    pub plans_rewrite: u64,
    /// Builds planned as a full walk (initial, forced, directory).
    pub plans_full: u64,
    /// Unchanged files analysed again because a build invalidated their
    /// import or reference edges.
    pub dependents_reresolved: u64,
    /// Such files left for later because a build had more than the bound.
    pub dependents_skipped: u64,
    /// Reference edges and occurrences written.
    pub references_written: u64,
    /// Edges and contracts the relation stage wrote.
    pub relation_rows_written: u64,
}

#[derive(Default)]
pub(crate) struct Counters {
    pub(crate) jobs_run: AtomicU64,
    pub(crate) files_read: AtomicU64,
    pub(crate) files_parsed: AtomicU64,
    pub(crate) chunks_written: AtomicU64,
    pub(crate) symbols_renamed: AtomicU64,
    pub(crate) embedding_calls: AtomicU64,
    pub(crate) inputs_embedded: AtomicU64,
    pub(crate) inputs_reused: AtomicU64,
    pub(crate) inputs_rejected: AtomicU64,
    pub(crate) generations_activated: AtomicU64,
    pub(crate) builds_superseded: AtomicU64,
    pub(crate) plans_incremental: AtomicU64,
    pub(crate) plans_rewrite: AtomicU64,
    pub(crate) plans_full: AtomicU64,
    pub(crate) dependents_reresolved: AtomicU64,
    pub(crate) dependents_skipped: AtomicU64,
    pub(crate) references_written: AtomicU64,
    pub(crate) relation_rows_written: AtomicU64,
}

impl Counters {
    pub(crate) fn add(counter: &AtomicU64, n: u64) {
        counter.fetch_add(n, Ordering::Relaxed);
    }

    fn snapshot(&self) -> IndexStats {
        let get = |c: &AtomicU64| c.load(Ordering::Relaxed);
        IndexStats {
            jobs_run: get(&self.jobs_run),
            files_read: get(&self.files_read),
            files_parsed: get(&self.files_parsed),
            chunks_written: get(&self.chunks_written),
            symbols_renamed: get(&self.symbols_renamed),
            embedding_calls: get(&self.embedding_calls),
            inputs_embedded: get(&self.inputs_embedded),
            inputs_reused: get(&self.inputs_reused),
            inputs_rejected: get(&self.inputs_rejected),
            generations_activated: get(&self.generations_activated),
            builds_superseded: get(&self.builds_superseded),
            plans_incremental: get(&self.plans_incremental),
            plans_rewrite: get(&self.plans_rewrite),
            plans_full: get(&self.plans_full),
            dependents_reresolved: get(&self.dependents_reresolved),
            dependents_skipped: get(&self.dependents_skipped),
            references_written: get(&self.references_written),
            relation_rows_written: get(&self.relation_rows_written),
        }
    }
}

/// In-memory artifacts handed from one stage of a build to the next. Purely
/// an accelerator: every stage can recompute what is missing.
#[derive(Default)]
pub(crate) struct BuildCache {
    builds: BTreeMap<(ViewId, i64), Artifacts>,
    bytes: usize,
}

#[derive(Default)]
pub(crate) struct Artifacts {
    pub(crate) texts: BTreeMap<ContentHash, Arc<str>>,
    pub(crate) analysed: BTreeMap<RepoPath, Arc<AnalysedFile>>,
    bytes: usize,
}

impl BuildCache {
    pub(crate) fn put_texts(
        &mut self,
        key: (ViewId, i64),
        texts: BTreeMap<ContentHash, Arc<str>>,
        cap: usize,
    ) {
        let weight: usize = texts.values().map(|t| t.len()).sum();
        if self.bytes.saturating_add(weight) > cap {
            return;
        }
        self.bytes += weight;
        let entry = self.builds.entry(key).or_default();
        entry.bytes += weight;
        entry.texts.extend(texts);
    }

    pub(crate) fn put_analysed(
        &mut self,
        key: (ViewId, i64),
        analysed: Vec<Arc<AnalysedFile>>,
        cap: usize,
    ) {
        let weight: usize = analysed.iter().map(|a| a.weight()).sum();
        let entry = self.builds.entry(key).or_default();
        // Texts are no longer needed once files are analysed.
        let freed = std::mem::take(&mut entry.texts)
            .values()
            .map(|t| t.len())
            .sum::<usize>();
        entry.bytes = entry.bytes.saturating_sub(freed);
        self.bytes = self.bytes.saturating_sub(freed);
        if self.bytes.saturating_add(weight) > cap {
            return;
        }
        self.bytes += weight;
        entry.bytes += weight;
        entry.analysed = analysed.into_iter().map(|a| (a.path.clone(), a)).collect();
    }

    pub(crate) fn get(&self, key: (ViewId, i64)) -> Option<&Artifacts> {
        self.builds.get(&key)
    }

    pub(crate) fn forget(&mut self, key: (ViewId, i64)) {
        if let Some(entry) = self.builds.remove(&key) {
            self.bytes = self.bytes.saturating_sub(entry.bytes);
        }
    }
}

/// One running job: its cancellation and attempt counters.
pub(crate) struct JobRun {
    pub(crate) attempt: u32,
    pub(crate) max_attempts: u32,
    pub(crate) cancel: CancellationToken,
    /// Mirrors `cancel` for blocking code (tree-sitter progress callbacks).
    pub(crate) flag: Arc<AtomicBool>,
}

impl JobRun {
    pub(crate) fn new(job: &Job, cancel: CancellationToken) -> Self {
        Self {
            attempt: job.attempts,
            max_attempts: job.max_attempts,
            cancel,
            flag: Arc::new(AtomicBool::new(false)),
        }
    }

    pub(crate) fn check(&self) -> Result<(), IndexError> {
        if self.cancel.is_cancelled() || self.flag.load(Ordering::Relaxed) {
            Err(IndexError::Cancelled)
        } else {
            Ok(())
        }
    }

    pub(crate) fn is_last_attempt(&self) -> bool {
        self.attempt >= self.max_attempts
    }
}

/// Shared state of an [`Indexer`].
pub(crate) struct Inner<E> {
    pub(crate) store: Store,
    pub(crate) config: IndexerConfig,
    pub(crate) providers: BTreeMap<Name, ProviderConfig>,
    pub(crate) embedders: BTreeMap<Name, Arc<E>>,
    pub(crate) relations: Arc<dyn RelationStage>,
    /// Whether a custom relation stage was given (it then gets the parsed
    /// changed files).
    pub(crate) relations_enabled: bool,
    /// The built-in, store-backed contract linking stage; when set it runs
    /// instead of `relations`.
    pub(crate) link: Option<Arc<LinkRelationStage>>,
    pub(crate) staleness: Option<mpsc::UnboundedSender<StalenessEvent>>,
    pub(crate) events: broadcast::Sender<ProgressEvent>,
    pub(crate) contexts: RwLock<BTreeMap<ViewId, Arc<ViewContext>>>,
    pub(crate) runtime: Mutex<Runtime>,
    pub(crate) lexical: Arc<LexicalStore>,
    pub(crate) overlays: Mutex<BTreeMap<ViewId, Arc<Overlay>>>,
    view_locks: Mutex<BTreeMap<ViewId, Arc<tokio::sync::Mutex<()>>>>,
    embed_locks: Mutex<BTreeMap<ViewId, Arc<tokio::sync::Mutex<()>>>>,
    pub(crate) cache: Mutex<BuildCache>,
    pub(crate) stats: Counters,
    /// Woken when this process enqueues a job.
    pub(crate) notify: Notify,
    /// Lease owner name of this process's workers.
    pub(crate) worker_id: String,
}

fn lock<T>(mutex: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl<E: Embedder + 'static> Inner<E> {
    pub(crate) fn context(&self, view: ViewId) -> Result<Arc<ViewContext>, IndexError> {
        self.contexts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&view)
            .cloned()
            .ok_or(IndexError::UnknownView(view))
    }

    pub(crate) fn contexts(&self) -> Vec<Arc<ViewContext>> {
        self.contexts
            .read()
            .unwrap_or_else(PoisonError::into_inner)
            .values()
            .cloned()
            .collect()
    }

    pub(crate) fn cache(&self) -> std::sync::MutexGuard<'_, BuildCache> {
        lock(&self.cache)
    }

    /// Serializes the stages of one view inside this process (other
    /// processes are fenced by the store).
    pub(crate) async fn view_lock(&self, view: ViewId) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = Arc::clone(lock(&self.view_locks).entry(view).or_default());
        mutex.lock_owned().await
    }

    /// Serializes the post-activation embedding runs of one view inside this
    /// process. Separate from [`Inner::view_lock`], so slow provider calls
    /// never hold up the next build's text, symbol and relation stages.
    pub(crate) async fn embed_lock(&self, view: ViewId) -> tokio::sync::OwnedMutexGuard<()> {
        let mutex = Arc::clone(lock(&self.embed_locks).entry(view).or_default());
        mutex.lock_owned().await
    }

    /// Which queued jobs this process may claim: those of the views it has
    /// registered (plus unscoped jobs left by earlier versions).
    pub(crate) fn claim_scope(&self) -> ClaimScope {
        ClaimScope {
            views: self.contexts().iter().map(|c| c.view).collect(),
            workspaces: Vec::new(),
            include_unscoped: true,
        }
    }

    pub(crate) fn emit(
        &self,
        ctx: &ViewContext,
        generation: Option<i64>,
        target: Option<&BuildTarget>,
        kind: ProgressKind,
    ) {
        // No receivers is fine: nobody is listening.
        let _ = self.events.send(ProgressEvent {
            view: ctx.view,
            project: ctx.project_name.clone(),
            generation,
            commit: target.and_then(BuildTarget::commit).map(str::to_owned),
            kind,
        });
    }

    pub(crate) fn with_runtime<R>(&self, view: ViewId, f: impl FnOnce(&mut ViewRuntime) -> R) -> R {
        let mut runtime = lock(&self.runtime);
        f(runtime.views.entry(view).or_default())
    }

    /// Records a tier change for the build of `generation` and broadcasts it.
    pub(crate) fn set_tier(
        &self,
        ctx: &ViewContext,
        generation: i64,
        target: &BuildTarget,
        tier: Tier,
        state: TierState,
    ) {
        self.with_runtime(ctx.view, |rt| rt.set_tier(generation, tier, state.clone()));
        self.emit(
            ctx,
            Some(generation),
            Some(target),
            ProgressKind::Tier { tier, state },
        );
    }

    pub(crate) fn record_error(&self, ctx: &ViewContext, generation: Option<i64>, reason: &str) {
        self.with_runtime(ctx.view, |rt| rt.last_error = Some(reason.to_owned()));
        self.emit(
            ctx,
            generation,
            None,
            ProgressKind::Failed {
                reason: reason.to_owned(),
            },
        );
    }

    pub(crate) fn wake_workers(&self) {
        self.notify.notify_waiters();
        self.notify.notify_one();
    }

    /// The T2 state implied by the configuration alone, for generations
    /// this process did not build.
    fn configured_t2(ctx: &ViewContext) -> Option<TierState> {
        match &ctx.embedding {
            EmbeddingDecision::Skip(reason) => Some(TierState::Skipped { reason: *reason }),
            EmbeddingDecision::Unavailable(reason) => Some(TierState::Failed {
                reason: reason.clone(),
            }),
            EmbeddingDecision::Embed { .. } => None,
        }
    }

    /// The key of the T2 job of `generation`, when its build target can be
    /// told from the store (the active commit, or the directory's tree).
    async fn embeddings_job_key(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        row: &View,
        generation: i64,
    ) -> Result<Option<String>, IndexError> {
        let target = match ctx.source_kind {
            SourceKind::Git => match &row.active_commit {
                Some(id) => BuildTarget::Commit { id: id.clone() },
                None => return Ok(None),
            },
            SourceKind::Directory => BuildTarget::Tree {
                hash: store_tree_hash(
                    conn,
                    GenerationPin {
                        view: ctx.view,
                        generation,
                    },
                )
                .await?,
            },
        };
        Ok(Some(stage_key(Tier::T2, ctx.view, &target, generation)))
    }

    /// Pending / running from the T2 job of `generation`, if it is live.
    async fn embeddings_job_state(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        row: &View,
        generation: i64,
    ) -> Result<Option<TierState>, IndexError> {
        let Some(key) = self.embeddings_job_key(conn, ctx, row, generation).await? else {
            return Ok(None);
        };
        Ok(
            match jobs::find_job_by_key(conn, &key).await?.map(|j| j.state) {
                Some(JobState::Queued | JobState::Failed) => Some(TierState::Pending),
                Some(JobState::Running) => Some(TierState::Running),
                _ => None,
            },
        )
    }

    /// The T2 state of the active generation `generation` as the store
    /// records it (for builds this process did not run).
    async fn stored_t2(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        row: &View,
        generation: i64,
    ) -> Result<TierState, IndexError> {
        if let Some(state) = Self::configured_t2(ctx) {
            return Ok(state);
        }
        // The profile the view serves (it may differ from the configured one
        // after a profile switch).
        let Some(profile) = self.embedding_targets(conn, ctx).await?.first().copied() else {
            return Ok(TierState::Pending);
        };
        let pin = GenerationPin {
            view: ctx.view,
            generation,
        };
        let incomplete = || TierState::Failed {
            reason: "embeddings of the active generation are incomplete".to_owned(),
        };
        let index_generation = embeddings::index_generation_at(conn, pin, profile).await?;
        Ok(match index_generation {
            Some(ig) => match ig.state {
                GenerationState::Active | GenerationState::Retired => TierState::Done,
                GenerationState::Failed => match ig.error {
                    Some(error) if error == BUDGET_EXHAUSTED => TierState::Skipped {
                        reason: TierSkip::BudgetExhausted,
                    },
                    Some(reason) => TierState::Failed { reason },
                    None => incomplete(),
                },
                GenerationState::Building => self
                    .embeddings_job_state(conn, ctx, row, generation)
                    .await?
                    .unwrap_or_else(incomplete),
            },
            None => self
                .embeddings_job_state(conn, ctx, row, generation)
                .await?
                .unwrap_or_else(incomplete),
        })
    }

    pub(crate) async fn status(&self, view: ViewId) -> Result<ViewStatus, IndexError> {
        let ctx = self.context(view)?;
        let mut conn = self.store.acquire().await?;
        let row = views::get_view(&mut conn, view)
            .await?
            .ok_or(IndexError::UnknownView(view))?;
        let building = views::building_generation(&mut conn, view).await?;
        let generations = views::list_generations(&mut conn, view).await?;
        // A failure newer than the active generation is the last error.
        let stored_error = generations
            .iter()
            .take_while(|g| Some(g.generation) != row.active_generation)
            .find(|g| g.state == GenerationState::Failed)
            .and_then(|g| g.error.clone());
        let runtime = self.with_runtime(view, |rt| rt.clone());
        let described = building.or(row.active_generation);
        let tiers = match described {
            Some(g) if Some(g) == row.active_generation => {
                // Activation means T0, T1 and T3 ran; T2 follows activation.
                let known = runtime.tiers(g);
                let t3 = match known.map(|t| &t.t3) {
                    Some(failed @ TierState::Failed { .. }) => failed.clone(),
                    _ => TierState::Done,
                };
                let t2 = match known.map(|t| &t.t2) {
                    Some(state) if *state != TierState::Pending => state.clone(),
                    _ => self.stored_t2(&mut conn, &ctx, &row, g).await?,
                };
                TierStates {
                    t0: TierState::Done,
                    t1: TierState::Done,
                    t2,
                    t3,
                }
            }
            Some(g) => runtime
                .tiers(g)
                .cloned()
                .unwrap_or_else(TierStates::pending),
            None => TierStates::pending(),
        };
        let behind = match (&row.latest_seen_commit, &row.active_commit) {
            (Some(seen), Some(active)) => seen != active,
            (Some(_), None) => true,
            _ => building.is_some(),
        };
        let lag = if behind {
            let since = runtime
                .behind_since
                .or_else(|| {
                    building.and_then(|b| {
                        generations
                            .iter()
                            .find(|g| g.generation == b)
                            .map(|g| g.created_at)
                    })
                })
                .unwrap_or(row.updated_at);
            let elapsed = OffsetDateTime::now_utc() - since;
            Some(Duration::try_from(elapsed).unwrap_or(Duration::ZERO))
        } else {
            None
        };
        let last_error = if runtime.observed {
            runtime.last_error.clone()
        } else {
            stored_error
        };
        Ok(ViewStatus {
            view,
            workspace: ctx.workspace_name.clone(),
            project: ctx.project_name.clone(),
            target: ctx.target.clone(),
            latest_seen_commit: row.latest_seen_commit,
            active_commit: row.active_commit,
            active_generation: row.active_generation,
            building_generation: building,
            tiers,
            lag,
            last_error,
        })
    }

    /// See [`Indexer::embedding_coverage`].
    pub(crate) async fn embedding_coverage(
        &self,
        view: ViewId,
    ) -> Result<Option<EmbeddingCoverage>, IndexError> {
        let ctx = self.context(view)?;
        let mut conn = self.store.acquire().await?;
        // Coverage of the profile the view serves.
        let Some(profile) = self
            .embedding_targets(&mut conn, &ctx)
            .await?
            .first()
            .copied()
        else {
            return Ok(None);
        };
        let row = views::get_view(&mut conn, view)
            .await?
            .ok_or(IndexError::UnknownView(view))?;
        let Some(generation) = row.active_generation else {
            return Ok(None);
        };
        let pin = GenerationPin { view, generation };
        let coverage =
            embeddings::input_coverage(&mut conn, pin, profile, &parser_version_tag()).await?;
        let complete = embeddings::active_index_generation(&mut conn, view, profile)
            .await?
            .is_some_and(|ig| ig.view_generation == generation);
        Ok(Some(EmbeddingCoverage {
            view,
            generation,
            profile,
            inputs: coverage.inputs,
            embedded: coverage.embedded,
            complete,
        }))
    }

    /// Queues the T2 of the active generation when it has neither vectors
    /// nor a live T2 job (a crash right after activation, or a generation
    /// activated by an older version). Returns whether a job was queued.
    pub(crate) async fn ensure_embeddings(&self, ctx: &ViewContext) -> Result<bool, IndexError> {
        let mut conn = self.store.acquire().await?;
        // The serving profile; a switch's target is caught up by
        // `reconcile_profiles`.
        let Some(profile) = self
            .embedding_targets(&mut conn, ctx)
            .await?
            .first()
            .copied()
        else {
            return Ok(false);
        };
        let Some(row) = views::get_view(&mut conn, ctx.view).await? else {
            return Ok(false);
        };
        let Some(generation) = row.active_generation else {
            return Ok(false);
        };
        let pin = GenerationPin {
            view: ctx.view,
            generation,
        };
        let started = embeddings::index_generation_at(&mut conn, pin, profile).await?;
        if started
            .as_ref()
            .is_some_and(|ig| ig.state != GenerationState::Building)
        {
            return Ok(false);
        }
        let Some(key) = self
            .embeddings_job_key(&mut conn, ctx, &row, generation)
            .await?
        else {
            return Ok(false);
        };
        if jobs::find_job_by_key(&mut conn, &key).await?.is_some() {
            return Ok(false);
        }
        let target = match &row.active_commit {
            Some(id) => BuildTarget::Commit { id: id.clone() },
            None => BuildTarget::Tree {
                hash: store_tree_hash(&mut conn, pin).await?,
            },
        };
        let payload = StagePayload {
            view: ctx.view,
            target,
            generation: Some(generation),
            priority: Priority::Background,
            force: false,
            profile: None,
            scip_import: None,
        };
        let queued = self
            .enqueue_next(&mut conn, Tier::T2, &payload, generation)
            .await?;
        Ok(queued.created)
    }
}

/// The indexing engine.
///
/// Cheap to clone (shared state). Generic over the [`Embedder`] type,
/// because the embedder trait is not object safe; use
/// [`knowell_embed::AnyEmbedder`] (the default) to select providers at run
/// time, or `Indexer::<AnyEmbedder>::builder` when no embedder is given.
pub struct Indexer<E: Embedder + 'static = AnyEmbedder> {
    pub(crate) inner: Arc<Inner<E>>,
}

impl<E: Embedder + 'static> Clone for Indexer<E> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
        }
    }
}

impl<E: Embedder + 'static> fmt::Debug for Indexer<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Indexer")
            .field("worker_id", &self.inner.worker_id)
            .field("data_dir", &self.inner.config.data_dir)
            .finish_non_exhaustive()
    }
}

/// Builds an [`Indexer`].
pub struct IndexerBuilder<E: Embedder + 'static> {
    store: Store,
    config: IndexerConfig,
    providers: BTreeMap<Name, ProviderConfig>,
    embedders: BTreeMap<Name, Arc<E>>,
    relations: Arc<dyn RelationStage>,
    relations_enabled: bool,
    link: LinkChoice,
    staleness: Option<mpsc::UnboundedSender<StalenessEvent>>,
}

/// Which link stage [`IndexerBuilder::build`] installs.
enum LinkChoice {
    /// The bundled packs, built at `build` (the default).
    Builtin,
    /// A configured stage.
    Given(Arc<LinkRelationStage>),
    /// None: a custom relation stage (possibly [`NoRelations`]) runs instead.
    Off,
}

impl<E: Embedder + 'static> fmt::Debug for IndexerBuilder<E> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("IndexerBuilder")
            .field("data_dir", &self.config.data_dir)
            .field("providers", &self.providers.keys().collect::<Vec<_>>())
            .field("embedders", &self.embedders.keys().collect::<Vec<_>>())
            .finish_non_exhaustive()
    }
}

impl<E: Embedder + 'static> IndexerBuilder<E> {
    /// Takes the embedding providers (kinds, default models) from the
    /// engine configuration. Without it, every configured provider is
    /// reported as undefined.
    pub fn engine(mut self, engine: &EngineConfig) -> Self {
        self.providers = engine.providers.clone();
        self
    }

    /// Supplies the embedder for the engine provider named `provider`.
    /// Projects naming that provider embed with it, after their data policy
    /// and the configured model and dimensions are checked against it.
    pub fn embedder(mut self, provider: Name, embedder: Arc<E>) -> Self {
        self.embedders.insert(provider, embedder);
        self
    }

    /// Replaces the T3 relation stage. The default is the built-in contract
    /// linking stage ([`LinkRelationStage`] with the bundled packs); pass
    /// `Arc::new(NoRelations)` to record no relations at all. A stage given
    /// here sees only the changed files of a build (through
    /// [`RelationStage::relate`]).
    pub fn relation_stage(mut self, stage: Arc<dyn RelationStage>) -> Self {
        self.relations = stage;
        self.relations_enabled = true;
        self.link = LinkChoice::Off;
        self
    }

    /// Uses this contract linking stage (for example with gateway path
    /// prefixes or extra packs) with the store-backed T3 path: whole-project
    /// extraction, linking across the workspace, symbol-level sources.
    pub fn link_stage(mut self, stage: Arc<LinkRelationStage>) -> Self {
        self.link = LinkChoice::Given(stage);
        self.relations = Arc::new(NoRelations);
        self.relations_enabled = false;
        self
    }

    /// Receives a [`StalenessEvent`] for every activated generation that
    /// changed or removed files.
    pub fn staleness_sender(mut self, sender: mpsc::UnboundedSender<StalenessEvent>) -> Self {
        self.staleness = Some(sender);
        self
    }

    /// Creates the data directory and the engine.
    ///
    /// # Errors
    /// [`IndexError::Io`] if the data directory cannot be created;
    /// [`IndexError::Config`] for unusable settings or bundled rule packs
    /// that fail validation.
    pub fn build(self) -> Result<Indexer<E>, IndexError> {
        if self.config.embedding.batch_size == 0 {
            return Err(IndexError::Config(
                "embedding.batch_size must be at least 1".to_owned(),
            ));
        }
        if self.config.jobs.max_attempts == 0 {
            return Err(IndexError::Config(
                "jobs.max_attempts must be at least 1".to_owned(),
            ));
        }
        if self.config.jobs.lease < Duration::from_millis(300) {
            return Err(IndexError::Config(
                "jobs.lease must be at least 300 ms".to_owned(),
            ));
        }
        let link = match self.link {
            LinkChoice::Builtin => {
                Some(Arc::new(LinkRelationStage::builtin().map_err(|e| {
                    IndexError::Config(format!("contract linking: {e}"))
                })?))
            }
            LinkChoice::Given(stage) => Some(stage),
            LinkChoice::Off => None,
        };
        std::fs::create_dir_all(&self.config.data_dir)
            .map_err(|e| IndexError::io("creating the data directory", e))?;
        let (events, _) = broadcast::channel(EVENT_CAPACITY);
        let worker_id = format!(
            "knowell-index-{}-{}",
            std::process::id(),
            uuid::Uuid::now_v7().simple()
        );
        Ok(Indexer {
            inner: Arc::new(Inner {
                lexical: Arc::new(LexicalStore::new(&self.config.data_dir)),
                store: self.store,
                config: self.config,
                providers: self.providers,
                embedders: self.embedders,
                relations: self.relations,
                relations_enabled: self.relations_enabled,
                link,
                staleness: self.staleness,
                events,
                contexts: RwLock::new(BTreeMap::new()),
                runtime: Mutex::new(Runtime::default()),
                overlays: Mutex::new(BTreeMap::new()),
                view_locks: Mutex::new(BTreeMap::new()),
                embed_locks: Mutex::new(BTreeMap::new()),
                cache: Mutex::new(BuildCache::default()),
                stats: Counters::default(),
                notify: Notify::new(),
                worker_id,
            }),
        })
    }
}

/// Result of [`Indexer::refresh_view`] / sync: what happened to a view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "outcome", rename_all = "snake_case")]
pub enum SyncOutcome {
    /// The active generation already matches the target.
    UpToDate {
        /// The view.
        view: ViewId,
        /// The active commit.
        commit: Option<String>,
    },
    /// A build of the target is queued (or was already).
    Queued {
        /// The view.
        view: ViewId,
        /// What is built.
        target: BuildTarget,
        /// The T0 job.
        job: JobId,
        /// Whether this call queued it (`false`: the same build was queued
        /// already).
        created: bool,
    },
    /// The target could not be resolved (missing branch, not a repository,
    /// ...). Nothing else is indexed in its place.
    Failed {
        /// The view.
        view: ViewId,
        /// Why.
        reason: String,
    },
}

/// What [`Indexer::run_until_idle`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize)]
pub struct RunSummary {
    /// Jobs claimed and run.
    pub jobs: u64,
    /// Jobs that succeeded (including superseded builds, which end quietly).
    pub succeeded: u64,
    /// Jobs that failed (retried later or dead-lettered).
    pub failed: u64,
}

impl<E: Embedder + 'static> Indexer<E> {
    /// Starts building an indexer on `store` with `config`.
    pub fn builder(store: Store, config: IndexerConfig) -> IndexerBuilder<E> {
        IndexerBuilder {
            store,
            config,
            providers: BTreeMap::new(),
            embedders: BTreeMap::new(),
            relations: Arc::new(NoRelations),
            relations_enabled: false,
            link: LinkChoice::Builtin,
            staleness: None,
        }
    }

    /// The store this indexer writes to.
    pub fn store(&self) -> &Store {
        &self.inner.store
    }

    /// The engine settings.
    pub fn config(&self) -> &IndexerConfig {
        &self.inner.config
    }

    /// Lease owner name of this indexer's job runs.
    pub fn worker_id(&self) -> &str {
        &self.inner.worker_id
    }

    /// Work counters since this indexer was built.
    pub fn stats(&self) -> IndexStats {
        self.inner.stats.snapshot()
    }

    /// Progress events of every view (tiers, activations, failures).
    pub fn subscribe(&self) -> broadcast::Receiver<ProgressEvent> {
        self.inner.events.subscribe()
    }

    /// Ensures the organization, workspace, sources, projects and views of
    /// `workspace` exist (idempotent: registering twice changes nothing) and
    /// registers the embedding profile of every project that embeds. The
    /// views become known to this indexer.
    ///
    /// # Errors
    /// Store errors; per-project problems are listed in
    /// [`Registration::issues`] instead.
    pub async fn register(
        &self,
        workspace: &ResolvedWorkspace,
    ) -> Result<Registration, IndexError> {
        let inner = &self.inner;
        let mut conn = inner.store.acquire().await?;
        let (registration, contexts) = register_workspace(
            &mut conn,
            &inner.config,
            &inner.providers,
            &inner.embedders,
            workspace,
        )
        .await?;
        let mut known = inner
            .contexts
            .write()
            .unwrap_or_else(PoisonError::into_inner);
        for context in contexts {
            known.insert(context.view, Arc::new(context));
        }
        Ok(registration)
    }

    /// Views registered with this indexer, by id.
    pub fn views(&self) -> Vec<ViewId> {
        self.inner.contexts().iter().map(|c| c.view).collect()
    }

    /// Registers `workspace`, queues a build of every view whose target
    /// moved, and runs the queue until it is idle (standalone mode), with
    /// [`IndexerConfig::concurrency`] jobs at once. Returns
    /// the sync outcome of every view, in registration order.
    ///
    /// # Errors
    /// Store errors. Views whose target cannot be resolved are reported as
    /// [`SyncOutcome::Failed`] and do not stop the others.
    pub async fn index_workspace(
        &self,
        workspace: &ResolvedWorkspace,
        priority: Priority,
    ) -> Result<(Registration, Vec<SyncOutcome>), IndexError> {
        let registration = self.register(workspace).await?;
        let mut outcomes = Vec::with_capacity(registration.views.len());
        for view in &registration.views {
            outcomes.push(self.refresh_view(view.view, priority).await?);
        }
        // Configuration changes become switches; building switches resume.
        self.inner.reconcile_profiles(true).await?;
        self.run_until_idle_with(self.inner.config.concurrency)
            .await?;
        Ok((registration, outcomes))
    }

    /// Resolves the view's target now and queues a build when it moved
    /// (a missing ref fails the view with the reason; no other ref is
    /// used). Does not wait for the build: run a [`crate::Worker`] or
    /// [`Indexer::run_until_idle`].
    ///
    /// # Errors
    /// [`IndexError::UnknownView`]; store errors.
    pub async fn refresh_view(
        &self,
        view: ViewId,
        priority: Priority,
    ) -> Result<SyncOutcome, IndexError> {
        self.inner.sync(view, priority, false).await
    }

    /// Like [`Indexer::refresh_view`], but builds every file again even if
    /// the target did not move (content, chunks and vectors are still reused
    /// by hash).
    ///
    /// # Errors
    /// As [`Indexer::refresh_view`].
    pub async fn rebuild_view(
        &self,
        view: ViewId,
        priority: Priority,
    ) -> Result<SyncOutcome, IndexError> {
        self.inner.sync(view, priority, true).await
    }

    /// Queues an `index.sync` job for the view (for triggers that should not
    /// resolve the target themselves, such as webhooks).
    ///
    /// # Errors
    /// Store errors.
    pub async fn enqueue_sync(
        &self,
        view: ViewId,
        priority: Priority,
    ) -> Result<JobId, IndexError> {
        let payload = crate::jobs::SyncPayload {
            view,
            priority,
            force: false,
        };
        let job = crate::jobs::sync_job(&payload, &self.inner.config.jobs)?;
        let mut conn = self.inner.store.acquire().await?;
        let queued = jobs::enqueue_scoped(&mut conn, &job, JobScope::View(view)).await?;
        self.inner.wake_workers();
        Ok(queued.id)
    }

    /// Freshness of one view.
    ///
    /// # Errors
    /// [`IndexError::UnknownView`]; store errors.
    pub async fn status(&self, view: ViewId) -> Result<ViewStatus, IndexError> {
        self.inner.status(view).await
    }

    /// Embedding coverage of the view's active generation in its profile:
    /// chunks with a vector out of the chunks meant to be embedded, and
    /// whether T2 finished for it. While T2 runs after an activation, queries
    /// can use this to say that semantic coverage is partial instead of
    /// presenting older vectors as current. `None` when the view does not
    /// embed (no provider, local-only policy, unusable configuration) or has
    /// no active generation.
    ///
    /// # Errors
    /// [`IndexError::UnknownView`]; store errors.
    pub async fn embedding_coverage(
        &self,
        view: ViewId,
    ) -> Result<Option<EmbeddingCoverage>, IndexError> {
        self.inner.embedding_coverage(view).await
    }

    /// Freshness of every registered view, by view id.
    ///
    /// # Errors
    /// Store errors.
    pub async fn statuses(&self) -> Result<Vec<ViewStatus>, IndexError> {
        let mut out = Vec::new();
        for view in self.views() {
            out.push(self.inner.status(view).await?);
        }
        Ok(out)
    }

    /// Claims and runs jobs one at a time, in queue order, until none is
    /// runnable (jobs waiting for a retry delay are left for later).
    /// Recovers expired leases of crashed workers first.
    ///
    /// # Errors
    /// Store errors while claiming; job failures are recorded on the job
    /// and do not end the run.
    pub async fn run_until_idle(&self) -> Result<RunSummary, IndexError> {
        self.run_until_idle_with(1).await
    }

    /// Like [`Indexer::run_until_idle`], with up to `concurrency` jobs
    /// running at once (stages of one view still run one after another).
    /// Returns when nothing is running and nothing is runnable.
    ///
    /// # Errors
    /// Store errors while claiming.
    pub async fn run_until_idle_with(&self, concurrency: usize) -> Result<RunSummary, IndexError> {
        self.run_until_idle_matching(concurrency, None).await
    }

    /// Like [`Indexer::run_until_idle_with`], restricted to the views registered
    /// when this call starts. Neither claims unscoped legacy jobs nor reclaims
    /// expired leases outside those views. Concurrent registration does not
    /// expand this run's scope. `concurrency` is a job count (at least one).
    ///
    /// # Errors
    /// Store errors while recovering leases or claiming jobs.
    pub async fn run_until_idle_scoped_with(
        &self,
        concurrency: usize,
    ) -> Result<RunSummary, IndexError> {
        let mut scope = self.inner.claim_scope();
        scope.include_unscoped = false;
        self.run_until_idle_matching(concurrency, Some(scope)).await
    }

    async fn run_until_idle_matching(
        &self,
        concurrency: usize,
        scope: Option<ClaimScope>,
    ) -> Result<RunSummary, IndexError> {
        let mut summary = RunSummary::default();
        {
            let mut conn = self.inner.store.acquire().await?;
            match scope.as_ref() {
                Some(scope) => jobs::reclaim_expired_leases_scoped(&mut conn, scope).await?,
                None => jobs::reclaim_expired_leases(&mut conn).await?,
            };
        }
        let mut running: tokio::task::JoinSet<bool> = tokio::task::JoinSet::new();
        loop {
            while running.len() < concurrency.max(1) {
                let claimed = match scope.as_ref() {
                    Some(scope) => self.claim_in_scope(scope).await?,
                    None => self.claim_next().await?,
                };
                let Some(job) = claimed else {
                    break;
                };
                summary.jobs += 1;
                running.spawn(crate::worker::execute(
                    Arc::clone(&self.inner),
                    job,
                    CancellationToken::new(),
                ));
            }
            // A finishing stage may queue the next one: claim again after.
            match running.join_next().await {
                None => break,
                Some(Ok(true)) => summary.succeeded += 1,
                Some(Ok(false)) => summary.failed += 1,
                Some(Err(error)) => return Err(error.into()),
            }
        }
        Ok(summary)
    }

    /// Claims and runs the single most urgent runnable job; `false` when
    /// nothing was runnable. Useful to step through a pipeline.
    ///
    /// # Errors
    /// Store errors while claiming.
    pub async fn run_next_job(&self) -> Result<bool, IndexError> {
        match self.claim_next().await? {
            Some(job) => {
                crate::worker::execute(Arc::clone(&self.inner), job, CancellationToken::new())
                    .await;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    async fn claim_next(&self) -> Result<Option<Job>, IndexError> {
        let scope = self.inner.claim_scope();
        self.claim_in_scope(&scope).await
    }

    async fn claim_in_scope(&self, scope: &ClaimScope) -> Result<Option<Job>, IndexError> {
        let mut conn = self.inner.store.acquire().await?;
        Ok(jobs::claim_scoped(
            &mut conn,
            &self.inner.worker_id,
            &JOB_KINDS,
            self.inner.config.jobs.lease,
            scope,
        )
        .await?)
    }

    /// The lexical index of the view's active generation (`None` before the
    /// first activation). Rebuilt from the store when its directory is
    /// missing.
    ///
    /// # Errors
    /// [`IndexError::UnknownView`]; store and lexical errors.
    pub async fn lexical(&self, view: ViewId) -> Result<Option<Arc<LexicalIndex>>, IndexError> {
        let ctx = self.inner.context(view)?;
        let mut conn = self.inner.store.acquire().await?;
        let Some(row) = views::get_view(&mut conn, view).await? else {
            return Err(IndexError::UnknownView(view));
        };
        let Some(active) = row.active_generation else {
            return Ok(None);
        };
        if let Some(index) = self.inner.lexical.get(view, active)? {
            return Ok(Some(index));
        }
        self.inner
            .rebuild_lexical(
                &mut conn,
                &ctx,
                GenerationPin {
                    view,
                    generation: active,
                },
            )
            .await?;
        self.inner.lexical.get(view, active)
    }

    /// The personal overlay last built for `view`, if any.
    pub fn overlay(&self, view: ViewId) -> Option<Arc<Overlay>> {
        lock(&self.inner.overlays).get(&view).cloned()
    }

    /// Builds the personal overlay of `worktree` over `view`: the worktree's
    /// committed and uncommitted differences from the view's active
    /// generation, parsed and lexically indexed in memory. The shared store
    /// is only read. The overlay replaces the previous one of the view.
    ///
    /// # Errors
    /// [`IndexError::UnknownView`]; git and lexical errors.
    pub async fn build_overlay(
        &self,
        view: ViewId,
        worktree: &Path,
    ) -> Result<Arc<Overlay>, IndexError> {
        self.inner.build_overlay(view, worktree).await
    }

    /// Drops the personal overlay of `view`.
    pub fn drop_overlay(&self, view: ViewId) {
        lock(&self.inner.overlays).remove(&view);
    }

    /// Cancels a queued or running job (a running job stops at its next
    /// check). Returns whether it was cancelled by this call.
    ///
    /// # Errors
    /// Store errors.
    pub async fn cancel_job(&self, job: JobId) -> Result<bool, IndexError> {
        let mut conn = self.inner.store.acquire().await?;
        Ok(jobs::cancel(&mut conn, job).await?)
    }
}

/// The skip reason of a tier, if it was skipped.
pub fn skip_reason(state: &TierState) -> Option<TierSkip> {
    match state {
        TierState::Skipped { reason } => Some(*reason),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_cache_respects_its_cap() {
        let mut cache = BuildCache::default();
        let key = (ViewId(uuid::Uuid::nil()), 1);
        let mut texts = BTreeMap::new();
        texts.insert(ContentHash::of(b"a"), Arc::<str>::from("x".repeat(10)));
        cache.put_texts(key, texts.clone(), 5);
        assert!(cache.get(key).is_none_or(|a| a.texts.is_empty()));
        cache.put_texts(key, texts, 100);
        assert_eq!(cache.get(key).map(|a| a.texts.len()), Some(1));
        cache.put_analysed(key, Vec::new(), 100);
        assert_eq!(cache.get(key).map(|a| a.texts.len()), Some(0));
        cache.forget(key);
        assert!(cache.get(key).is_none());
        assert_eq!(cache.bytes, 0);
    }

    #[test]
    fn skip_reason_extracts() {
        assert_eq!(
            skip_reason(&TierState::Skipped {
                reason: TierSkip::NoProvider
            }),
            Some(TierSkip::NoProvider)
        );
        assert_eq!(skip_reason(&TierState::Done), None);
    }
}
