//! The stages of a build as run by the job runner: sync and T0 here, T1 in
//! `symbols_stage`, T3 (relations and activation) in `relations_stage`, and
//! the post-activation T2 in `embeddings_stage`.
//!
//! Order: T0 text → T1 symbols → T3 relations → **activate** → T2
//! embeddings. Lexical, symbol and graph search use a new generation as soon
//! as T3 activated it; vectors are enrichment that follows.
//!
//! Every stage of generation `g` before activation first checks that `g` is
//! still building and that the view's target has not moved on; every store
//! write it makes is fenced by the store as well. A stage that finds itself
//! replaced ends with [`JobOutcome::Superseded`] instead of failing, so a
//! late job of an older build never retries, never writes and never
//! activates. T2 instead checks that `g` is still the active generation.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;

use knowell_core::{ContentHash, RepoPath};
use knowell_embed::Embedder;
use knowell_parse::Language;
use knowell_source::{FileRead, SourceFile};
use knowell_store::content::{self, FileChange, NewContent};
use knowell_store::embeddings;
use knowell_store::jobs::{self, Enqueued, Job, JobScope};
use knowell_store::views::{self, GenerationPin, View};
use knowell_store::{GenerationState, PgConnection, SourceKind, StoreError};

use crate::analyze::{AnalyseOptions, AnalysedFile, analyse};
use crate::config::Priority;
use crate::context::{EmbeddingDecision, ViewContext};
use crate::error::IndexError;
use crate::indexer::{Counters, Inner, JobRun, SyncOutcome};
use crate::jobs::{
    BuildTarget, JOB_EMBEDDINGS, JOB_RELATIONS, JOB_SYMBOLS, JOB_SYNC, JOB_TEXT, StagePayload,
    SyncPayload, decode, stage_job, text_job,
};
use crate::lexical::LexicalUpdate;
use crate::manifest;
use crate::plan::{self, PlanInput, PlanKind};
use crate::relate::StaleFile;
use crate::status::{ProgressKind, Tier, TierState};

/// How a job ended when it did not fail.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum JobOutcome {
    /// The stage did its work.
    Completed,
    /// A newer build replaced this one; nothing more to do.
    Superseded(String),
}

/// Why files are analysed: decides whether the build cache may answer and
/// whether identifier uses are listed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Purpose {
    /// T1: always parse; list identifiers for reference resolution.
    Symbols,
    /// T3 custom stages: reuse T1's analysis when cached.
    Relations,
    /// T2: rebuild embedding inputs of files the cache does not hold.
    Embeddings,
}

impl Purpose {
    fn prefer_cache(self) -> bool {
        self == Purpose::Relations
    }

    fn identifiers(self) -> bool {
        self == Purpose::Symbols
    }
}

/// A path added, modified or renamed in a generation.
#[derive(Debug, Clone)]
pub(crate) struct Upsert {
    pub(crate) path: RepoPath,
    pub(crate) hash: ContentHash,
    pub(crate) renamed_from: Option<RepoPath>,
}

/// What a generation changed relative to the active one, read from the
/// store (so every stage, in any process, sees the same set).
#[derive(Debug, Clone, Default)]
pub(crate) struct Delta {
    pub(crate) upserts: Vec<Upsert>,
    /// Paths new in the view (added, or new paths of renames).
    pub(crate) added: Vec<RepoPath>,
    /// Paths gone from the view (deleted, or old paths of renames).
    pub(crate) removed: Vec<RepoPath>,
    /// Previous versions that changed or disappeared.
    pub(crate) stale: Vec<StaleFile>,
}

pub(crate) async fn files_map(
    conn: &mut PgConnection,
    pin: GenerationPin,
) -> Result<BTreeMap<RepoPath, ContentHash>, IndexError> {
    Ok(content::files_at(conn, pin)
        .await?
        .into_iter()
        .map(|f| (f.path, f.content_hash))
        .collect())
}

/// The changes of generation `generation` relative to `active`.
pub(crate) async fn derive_delta(
    conn: &mut PgConnection,
    view: knowell_store::ViewId,
    generation: i64,
    active: Option<i64>,
) -> Result<Delta, IndexError> {
    let new = content::files_at(conn, GenerationPin { view, generation }).await?;
    let old = match active {
        Some(a) => {
            files_map(
                conn,
                GenerationPin {
                    view,
                    generation: a,
                },
            )
            .await?
        }
        None => BTreeMap::new(),
    };
    let new_paths: BTreeMap<&RepoPath, &ContentHash> =
        new.iter().map(|f| (&f.path, &f.content_hash)).collect();
    let mut delta = Delta::default();
    let mut renamed_to: BTreeMap<RepoPath, (RepoPath, ContentHash)> = BTreeMap::new();
    for file in &new {
        if old.get(&file.path) == Some(&file.content_hash) {
            continue;
        }
        // A rename recorded by this generation (not an older one).
        let renamed_from = file
            .renamed_from
            .clone()
            .filter(|_| file.valid_from == generation)
            .filter(|from| old.contains_key(from) && !new_paths.contains_key(from));
        if let Some(from) = &renamed_from {
            renamed_to.insert(from.clone(), (file.path.clone(), file.content_hash));
        }
        if !old.contains_key(&file.path) {
            delta.added.push(file.path.clone());
        }
        delta.upserts.push(Upsert {
            path: file.path.clone(),
            hash: file.content_hash,
            renamed_from,
        });
    }
    for (path, old_hash) in &old {
        match new_paths.get(path) {
            Some(hash) if *hash == old_hash => {}
            Some(hash) => delta.stale.push(StaleFile {
                path: path.clone(),
                old_hash: *old_hash,
                new_hash: Some(**hash),
                renamed_to: None,
            }),
            None => {
                delta.removed.push(path.clone());
                let (renamed, new_hash) = match renamed_to.get(path) {
                    Some((to, hash)) => (Some(to.clone()), Some(*hash)),
                    None => (None, None),
                };
                delta.stale.push(StaleFile {
                    path: path.clone(),
                    old_hash: *old_hash,
                    new_hash,
                    renamed_to: renamed,
                });
            }
        }
    }
    Ok(delta)
}

/// Tree hash of the store's files of a generation.
pub(crate) async fn store_tree_hash(
    conn: &mut PgConnection,
    pin: GenerationPin,
) -> Result<ContentHash, IndexError> {
    let files = files_map(conn, pin).await?;
    Ok(crate::merkle::tree_hash(files.iter()))
}

/// The reason a build of `target` is obsolete because the view's target
/// moved on, if it is.
pub(crate) fn moved_on(row: &View, target: &BuildTarget) -> Option<String> {
    let commit = target.commit()?;
    match row.latest_seen_commit.as_deref() {
        Some(seen) if seen == commit => None,
        Some(seen) => Some(format!("the target moved on to {seen}")),
        None => Some("the target has no seen commit".to_owned()),
    }
}

fn language_of(path: &RepoPath, text: &str) -> String {
    Language::detect(path, text).as_str().to_owned()
}

impl<E: Embedder + 'static> Inner<E> {
    /// Runs one claimed job.
    pub(crate) async fn run_job(
        self: &Arc<Self>,
        job: &Job,
        run: &JobRun,
    ) -> Result<JobOutcome, IndexError> {
        let result = match job.kind.as_str() {
            JOB_SYNC => {
                let payload: SyncPayload = decode(&job.payload)?;
                self.sync(payload.view, payload.priority, payload.force)
                    .await
                    .map(|_| JobOutcome::Completed)
            }
            JOB_TEXT => self.stage_text(decode(&job.payload)?, run).await,
            JOB_SYMBOLS => self.stage_symbols(decode(&job.payload)?, run).await,
            JOB_EMBEDDINGS => self.stage_embeddings(decode(&job.payload)?, run).await,
            JOB_RELATIONS => self.stage_relations(decode(&job.payload)?, run).await,
            other => Err(IndexError::invalid(
                "job kind",
                format!("`{other}` is not an indexing job"),
            )),
        };
        match result {
            // A fenced write or activation lost against a newer build.
            Err(error) if error.is_superseded() => Ok(JobOutcome::Superseded(error.to_string())),
            other => other,
        }
    }

    /// Resolves the target: a commit for git sources, the tree hash for
    /// directories. `Ok(Err(reason))` when it cannot be resolved.
    async fn resolve_target(
        &self,
        ctx: &Arc<ViewContext>,
    ) -> Result<Result<BuildTarget, String>, IndexError> {
        let ctx = Arc::clone(ctx);
        let limits = self.config.limits;
        let mode = self.config.git_config;
        let resolved = tokio::task::spawn_blocking(move || match ctx.source_kind {
            SourceKind::Directory => plan::directory_tree_hash(&ctx, &limits)
                .map(|hash| BuildTarget::Tree { hash })
                .map_err(|e| e.to_string()),
            SourceKind::Git => plan::open_repo(&ctx.source_path, mode)
                .and_then(|repo| repo.resolve(&ctx.target))
                .map(|r| BuildTarget::Commit { id: r.commit })
                .map_err(|e| e.to_string()),
        })
        .await?;
        Ok(resolved)
    }

    /// Durably records that the view's target cannot be built: the building
    /// generation (if any) fails with the reason, otherwise an empty
    /// generation is opened and failed so the reason survives restarts.
    /// Repeating the same failure records nothing new.
    pub(crate) async fn record_target_failure(
        &self,
        ctx: &ViewContext,
        reason: &str,
    ) -> Result<(), IndexError> {
        let mut conn = self.store.acquire().await?;
        let generations = views::list_generations(&mut conn, ctx.view).await?;
        let repeated = generations.first().is_some_and(|g| {
            g.state == GenerationState::Failed && g.error.as_deref() == Some(reason)
        });
        if !repeated {
            match views::building_generation(&mut conn, ctx.view).await? {
                Some(g) => match views::fail_generation(&mut conn, ctx.view, g, reason).await {
                    Ok(()) | Err(StoreError::GenerationNotBuilding { .. }) => {
                        self.cache().forget((ctx.view, g));
                    }
                    Err(e) => return Err(e.into()),
                },
                None => match views::begin_generation(&mut conn, ctx.view, None).await {
                    Ok(g) => views::fail_generation(&mut conn, ctx.view, g, reason).await?,
                    Err(StoreError::GenerationBusy { .. }) => {}
                    Err(e) => return Err(e.into()),
                },
            }
        }
        self.with_runtime(ctx.view, |rt| rt.observed = true);
        self.record_error(ctx, None, reason);
        Ok(())
    }

    /// Resolves the view's target, records the seen commit and queues a T0
    /// job when the active generation does not match it.
    pub(crate) async fn sync(
        &self,
        view: knowell_store::ViewId,
        priority: Priority,
        force: bool,
    ) -> Result<SyncOutcome, IndexError> {
        let ctx = self.context(view)?;
        let target = match self.resolve_target(&ctx).await? {
            Ok(target) => target,
            Err(reason) => {
                self.record_target_failure(&ctx, &reason).await?;
                return Ok(SyncOutcome::Failed { view, reason });
            }
        };
        let mut conn = self.store.acquire().await?;
        if let Some(commit) = target.commit() {
            views::record_seen_commit(&mut conn, view, commit).await?;
        }
        let row = views::get_view(&mut conn, view)
            .await?
            .ok_or(IndexError::UnknownView(view))?;
        let building = views::building_generation(&mut conn, view).await?;
        let current = match &target {
            BuildTarget::Commit { id } => row.active_commit.as_deref() == Some(id.as_str()),
            BuildTarget::Tree { hash } => match row.active_generation {
                Some(generation) => {
                    store_tree_hash(&mut conn, GenerationPin { view, generation }).await? == *hash
                }
                None => false,
            },
        };
        if current && !force {
            self.with_runtime(view, |rt| {
                rt.observed = true;
                rt.last_error = None;
                if building.is_none() {
                    rt.behind_since = None;
                }
            });
            self.emit(
                &ctx,
                row.active_generation,
                Some(&target),
                ProgressKind::UpToDate,
            );
            return Ok(SyncOutcome::UpToDate {
                view,
                commit: row.active_commit,
            });
        }
        self.with_runtime(view, |rt| {
            rt.observed = true;
            rt.last_error = None;
            rt.behind_since
                .get_or_insert_with(time::OffsetDateTime::now_utc);
        });
        let payload = StagePayload {
            view,
            target: target.clone(),
            generation: None,
            priority,
            force,
        };
        let mut job = text_job(&payload, row.last_generation, &self.config.jobs)?;
        let mut queued = jobs::enqueue_scoped(&mut conn, &job, JobScope::View(view)).await?;
        if !queued.created {
            // The same build was queued before. If that job already ended
            // without leaving a build of this target behind (it found the
            // target had moved on, and then it moved back), queue a new one.
            let existing = jobs::get_job(&mut conn, queued.id).await?;
            let ended = existing.is_some_and(|j| {
                matches!(
                    j.state,
                    knowell_store::JobState::Succeeded
                        | knowell_store::JobState::Cancelled
                        | knowell_store::JobState::Dead
                )
            });
            let in_progress = match building {
                Some(g) => views::get_generation(&mut conn, view, g)
                    .await?
                    .is_some_and(|info| info.resolved_commit.as_deref() == target.commit()),
                None => false,
            };
            if ended && !in_progress {
                job.idempotency_key = job
                    .idempotency_key
                    .map(|key| format!("{key}:retry-{}", uuid::Uuid::now_v7().simple()));
                queued = jobs::enqueue_scoped(&mut conn, &job, JobScope::View(view)).await?;
            }
        }
        if queued.created {
            self.wake_workers();
            self.emit(&ctx, None, Some(&target), ProgressKind::Queued);
        }
        Ok(SyncOutcome::Queued {
            view,
            target,
            job: queued.id,
            created: queued.created,
        })
    }

    /// Queues the stage producing `tier` for `generation`, attributed to the
    /// view (so only workers serving the view claim it).
    pub(crate) async fn enqueue_next(
        &self,
        conn: &mut PgConnection,
        tier: Tier,
        payload: &StagePayload,
        generation: i64,
    ) -> Result<Enqueued, IndexError> {
        let job = stage_job(tier, payload, generation, &self.config.jobs)?;
        let queued = jobs::enqueue_scoped(conn, &job, JobScope::View(payload.view)).await?;
        self.wake_workers();
        Ok(queued)
    }

    /// `Ok(active)` while generation `g` is building and the target has not
    /// moved on; `Err(reason)` when the build is obsolete (and fails it).
    pub(crate) async fn check_building(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        payload: &StagePayload,
        generation: i64,
    ) -> Result<Result<Option<i64>, String>, IndexError> {
        let Some(info) = views::get_generation(conn, ctx.view, generation).await? else {
            return Ok(Err(format!("generation {generation} no longer exists")));
        };
        if info.state != GenerationState::Building {
            return Ok(Err(format!("generation {generation} is {}", info.state)));
        }
        let row = views::get_view(conn, ctx.view)
            .await?
            .ok_or(IndexError::UnknownView(ctx.view))?;
        if let Some(reason) = moved_on(&row, &payload.target) {
            match views::fail_generation(conn, ctx.view, generation, &reason).await {
                Ok(()) | Err(StoreError::GenerationNotBuilding { .. }) => {}
                Err(e) => return Err(e.into()),
            }
            self.cache().forget((ctx.view, generation));
            return Ok(Err(reason));
        }
        Ok(Ok(row.active_generation))
    }

    async fn begin(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        target: &BuildTarget,
    ) -> Result<i64, IndexError> {
        match views::begin_generation(conn, ctx.view, target.commit()).await {
            Ok(g) => Ok(g),
            Err(StoreError::GenerationBusy { building, .. }) => {
                Err(IndexError::Inconsistent(format!(
                    "generation {building} of project `{}` started building concurrently; retrying",
                    ctx.project_name
                )))
            }
            Err(e) => Err(e.into()),
        }
    }

    /// Fills `texts` with the redacted text of `hashes` from the store (one
    /// round trip per few thousand blobs).
    pub(crate) async fn fill_texts(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        hashes: impl IntoIterator<Item = ContentHash>,
        texts: &mut BTreeMap<ContentHash, Arc<str>>,
    ) -> Result<(), IndexError> {
        let wanted: Vec<ContentHash> = hashes
            .into_iter()
            .filter(|h| !texts.contains_key(h))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        if wanted.is_empty() {
            return Ok(());
        }
        for (hash, text) in content::redacted_texts(conn, ctx.organization, &wanted).await? {
            texts.insert(hash, Arc::from(text));
        }
        Ok(())
    }

    /// Reads the given project paths of the build target again (blocking).
    async fn read_paths(
        &self,
        ctx: &Arc<ViewContext>,
        target: &BuildTarget,
        paths: Vec<RepoPath>,
    ) -> Result<Vec<SourceFile>, IndexError> {
        let ctx = Arc::clone(ctx);
        let target = target.clone();
        let limits = self.config.limits;
        let mode = self.config.git_config;
        let files = tokio::task::spawn_blocking(move || -> Result<Vec<SourceFile>, IndexError> {
            match &target {
                BuildTarget::Commit { id } => {
                    let repo = plan::open_repo(&ctx.source_path, mode)?;
                    let repo_paths: Vec<RepoPath> =
                        paths.iter().filter_map(|p| ctx.to_repo_path(p)).collect();
                    let report = repo.read_commit_files(
                        id,
                        &repo_paths,
                        &ctx.policy,
                        &knowell_source::WalkOptions {
                            max_file_bytes: limits.max_file_bytes,
                            respect_gitignore: false,
                            follow_symlinks: false,
                        },
                    )?;
                    Ok(report
                        .files
                        .into_iter()
                        .filter_map(|f| {
                            let path = ctx.to_project_path(&f.path)?;
                            Some(SourceFile { path, ..f })
                        })
                        .collect())
                }
                BuildTarget::Tree { .. } => {
                    let root = ctx.project_dir();
                    let options = knowell_source::WalkOptions {
                        max_file_bytes: limits.max_file_bytes,
                        respect_gitignore: false,
                        follow_symlinks: false,
                    };
                    let mut files = Vec::with_capacity(paths.len());
                    for path in &paths {
                        if let FileRead::File(file) =
                            knowell_source::read_file(&root, path, &ctx.policy, &options)?
                        {
                            files.push(file);
                        }
                    }
                    Ok(files)
                }
            }
        })
        .await??;
        Counters::add(&self.stats.files_read, files.len() as u64);
        Ok(files)
    }

    /// T0: begin (or resume) the generation, plan, store content and file
    /// versions, build the lexical index, queue T1.
    async fn stage_text(
        self: &Arc<Self>,
        p: StagePayload,
        run: &JobRun,
    ) -> Result<JobOutcome, IndexError> {
        let ctx = self.context(p.view)?;
        let _guard = self.view_lock(p.view).await;
        let mut conn = self.store.acquire().await?;
        let row = views::get_view(&mut conn, p.view)
            .await?
            .ok_or(IndexError::UnknownView(p.view))?;
        if let Some(reason) = moved_on(&row, &p.target) {
            return Ok(JobOutcome::Superseded(reason));
        }
        let generation = match views::building_generation(&mut conn, p.view).await? {
            Some(g) => {
                let same = p.target.commit().is_some()
                    && views::get_generation(&mut conn, p.view, g)
                        .await?
                        .is_some_and(|info| info.resolved_commit.as_deref() == p.target.commit());
                if same && !p.force {
                    // Resume after a crash: every write below replaces the
                    // earlier attempt of this generation.
                    g
                } else {
                    let reason = format!("superseded by a build of {}", p.target.key());
                    match views::fail_generation(&mut conn, p.view, g, &reason).await {
                        Ok(()) | Err(StoreError::GenerationNotBuilding { .. }) => {}
                        Err(e) => return Err(e.into()),
                    }
                    self.cache().forget((p.view, g));
                    Counters::add(&self.stats.builds_superseded, 1);
                    self.emit(&ctx, Some(g), None, ProgressKind::Superseded { reason });
                    self.begin(&mut conn, &ctx, &p.target).await?
                }
            }
            None => self.begin(&mut conn, &ctx, &p.target).await?,
        };
        self.set_tier(&ctx, generation, &p.target, Tier::T0, TierState::Running);
        run.check()?;

        let active = row.active_generation;
        let base_files = match active {
            Some(a) => {
                files_map(
                    &mut conn,
                    GenerationPin {
                        view: p.view,
                        generation: a,
                    },
                )
                .await?
            }
            None => BTreeMap::new(),
        };
        let base_manifest = active.and_then(|a| manifest::load(&self.config.data_dir, p.view, a));
        // A different content policy (excludes, size limit, root) changes
        // which unchanged files belong to the view: plan from scratch.
        let policy_changed = base_manifest
            .as_ref()
            .is_some_and(|m| m.policy != ctx.policy_key);
        let base_manifest = base_manifest.filter(|m| m.policy == ctx.policy_key);
        let plan = {
            let ctx = Arc::clone(&ctx);
            let target = p.target.clone();
            let base_commit = row.active_commit.clone();
            let limits = self.config.limits;
            let git_config = self.config.git_config;
            let force = p.force || policy_changed;
            tokio::task::spawn_blocking(move || {
                plan::plan(&PlanInput {
                    ctx: &ctx,
                    generation,
                    target: &target,
                    base_files: &base_files,
                    base_commit: base_commit.as_deref(),
                    base_manifest: base_manifest.as_ref(),
                    limits,
                    force,
                    git_config,
                })
            })
            .await??
        };
        let counter = match plan.kind {
            PlanKind::Incremental => &self.stats.plans_incremental,
            PlanKind::Rewrite => &self.stats.plans_rewrite,
            PlanKind::Initial | PlanKind::Rebuild | PlanKind::Directory => &self.stats.plans_full,
        };
        Counters::add(counter, 1);
        Counters::add(&self.stats.files_read, plan.files.len() as u64);
        tracing::debug!(
            project = %ctx.project_name,
            generation,
            kind = ?plan.kind,
            changes = plan.changes.len(),
            read = plan.files.len(),
            skipped = plan.skipped,
            "planned"
        );
        run.check()?;

        // Content rows: only redacted text is ever stored.
        let mut texts: BTreeMap<ContentHash, Arc<str>> = BTreeMap::new();
        let mut sizes: BTreeMap<ContentHash, (u64, RepoPath)> = BTreeMap::new();
        for file in plan.files.values() {
            texts.insert(file.hash, Arc::from(file.text.as_str()));
            sizes.insert(file.hash, (file.size, file.path.clone()));
        }
        let upserts: Vec<(&RepoPath, ContentHash)> = plan
            .changes
            .iter()
            .filter_map(|c| match c {
                FileChange::Upsert {
                    path, content_hash, ..
                } => Some((path, *content_hash)),
                FileChange::Delete { .. } => None,
            })
            .collect();
        let hashes: Vec<ContentHash> = upserts.iter().map(|(_, h)| *h).collect();
        let missing = content::missing_contents(&mut conn, ctx.organization, &hashes).await?;
        let unread: Vec<RepoPath> = {
            let missing_set: BTreeSet<&ContentHash> = missing.iter().collect();
            upserts
                .iter()
                .filter(|(_, h)| missing_set.contains(h) && !texts.contains_key(h))
                .map(|(path, _)| (*path).clone())
                .collect()
        };
        if !unread.is_empty() {
            // The blob cache knew these blobs, but the store lacks their
            // content (for example a restored database): read them again.
            for file in self.read_paths(&ctx, &p.target, unread).await? {
                texts.insert(file.hash, Arc::from(file.text.as_str()));
                sizes.insert(file.hash, (file.size, file.path.clone()));
            }
        }
        let mut new_contents = Vec::with_capacity(missing.len());
        for hash in &missing {
            let (Some(text), Some((size, path))) = (texts.get(hash), sizes.get(hash)) else {
                return Err(IndexError::Inconsistent(format!(
                    "content {} of project `{}` is neither stored nor readable",
                    hash.short(),
                    ctx.project_name
                )));
            };
            new_contents.push(NewContent {
                hash: *hash,
                size_bytes: *size,
                language: Some(language_of(path, text)),
                redacted_text: Some(text.to_string()),
            });
        }
        content::upsert_contents(&mut conn, ctx.organization, &new_contents).await?;
        content::apply_file_changes(&mut conn, p.view, generation, &plan.changes).await?;
        run.check()?;

        // Lexical index: copy-on-write from the active generation, or a
        // rebuild from the store when there is no usable base on disk.
        let base_lexical = active.filter(|a| self.lexical.is_complete(p.view, *a));
        let update = if base_lexical.is_some() || active.is_none() {
            let mut deletes = Vec::new();
            let mut adds = Vec::new();
            self.fill_texts(&mut conn, &ctx, hashes.iter().copied(), &mut texts)
                .await?;
            for change in &plan.changes {
                match change {
                    FileChange::Upsert {
                        path,
                        content_hash,
                        renamed_from,
                    } => {
                        if let Some(from) = renamed_from {
                            deletes.push(from.to_string());
                        }
                        if let Some(text) = texts.get(content_hash) {
                            adds.push((path.to_string(), Arc::clone(text)));
                        }
                    }
                    FileChange::Delete { path } => deletes.push(path.to_string()),
                }
            }
            LexicalUpdate { deletes, adds }
        } else {
            self.full_lexical_update(
                &mut conn,
                &ctx,
                GenerationPin {
                    view: p.view,
                    generation,
                },
                &mut texts,
            )
            .await?
        };
        let lexical = Arc::clone(&self.lexical);
        let view = p.view;
        tokio::task::spawn_blocking(move || lexical.build(view, generation, base_lexical, &update))
            .await??;
        manifest::save(&self.config.data_dir, &plan.manifest)?;

        let changed: BTreeSet<ContentHash> = hashes.iter().copied().collect();
        texts.retain(|h, _| changed.contains(h));
        self.cache()
            .put_texts((p.view, generation), texts, self.config.build_cache_bytes);
        self.enqueue_next(&mut conn, Tier::T1, &p, generation)
            .await?;
        self.set_tier(&ctx, generation, &p.target, Tier::T0, TierState::Done);
        Ok(JobOutcome::Completed)
    }

    /// Every file of `pin` as lexical documents (texts from the store).
    async fn full_lexical_update(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        pin: GenerationPin,
        texts: &mut BTreeMap<ContentHash, Arc<str>>,
    ) -> Result<LexicalUpdate, IndexError> {
        let files = content::files_at(conn, pin).await?;
        self.fill_texts(conn, ctx, files.iter().map(|f| f.content_hash), texts)
            .await?;
        let adds = files
            .iter()
            .filter_map(|f| {
                texts
                    .get(&f.content_hash)
                    .map(|t| (f.path.to_string(), Arc::clone(t)))
            })
            .collect();
        Ok(LexicalUpdate {
            deletes: Vec::new(),
            adds,
        })
    }

    /// Rebuilds the lexical index of `pin` from the store's text.
    pub(crate) async fn rebuild_lexical(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        pin: GenerationPin,
    ) -> Result<(), IndexError> {
        let mut texts = BTreeMap::new();
        let update = self.full_lexical_update(conn, ctx, pin, &mut texts).await?;
        let lexical = Arc::clone(&self.lexical);
        tokio::task::spawn_blocking(move || lexical.build(pin.view, pin.generation, None, &update))
            .await??;
        Ok(())
    }

    /// Parses `items` (path, content hash, text) on blocking threads.
    pub(crate) async fn analyse_items(
        &self,
        ctx: &ViewContext,
        items: Vec<(RepoPath, ContentHash, Arc<str>)>,
        identifiers: bool,
        run: &JobRun,
    ) -> Result<Vec<Arc<AnalysedFile>>, IndexError> {
        let parallel = self.config.parse_parallelism.max(1);
        let per_task = items.len().div_ceil(parallel).max(1);
        let options = AnalyseOptions {
            chunking: self.config.chunking,
            limits: self.config.parse_limits,
            generated: self.config.content.generated,
            identifiers,
        };
        let project = ctx.project_name.to_string();
        let mut groups: Vec<Vec<(RepoPath, ContentHash, Arc<str>)>> = Vec::new();
        let mut items = items.into_iter().peekable();
        while items.peek().is_some() {
            groups.push(items.by_ref().take(per_task).collect());
        }
        let mut tasks = Vec::with_capacity(groups.len());
        for group in groups {
            let flag = Arc::clone(&run.flag);
            let project = project.clone();
            tasks.push(tokio::task::spawn_blocking(move || {
                let mut out = Vec::with_capacity(group.len());
                for (path, hash, text) in group {
                    if flag.load(std::sync::atomic::Ordering::Relaxed) {
                        break;
                    }
                    out.push(Arc::new(analyse(
                        &project, &path, hash, text, &options, &flag,
                    )));
                }
                out
            }));
        }
        let mut analysed = Vec::new();
        for task in tasks {
            analysed.extend(task.await?);
        }
        run.check()?;
        Counters::add(&self.stats.files_parsed, analysed.len() as u64);
        Ok(analysed)
    }

    /// Analysis of the upserted files of a build: from the build cache, or
    /// parsed again from stored text.
    pub(crate) async fn analysed_upserts(
        &self,
        conn: &mut PgConnection,
        ctx: &ViewContext,
        generation: i64,
        upserts: &[Upsert],
        run: &JobRun,
        purpose: Purpose,
    ) -> Result<BTreeMap<RepoPath, Arc<AnalysedFile>>, IndexError> {
        let prefer_cache = purpose.prefer_cache();
        let key = (ctx.view, generation);
        let (cached, mut texts) = {
            let cache = self.cache();
            match cache.get(key) {
                Some(a) => (a.analysed.clone(), a.texts.clone()),
                None => (BTreeMap::new(), BTreeMap::new()),
            }
        };
        let mut out = BTreeMap::new();
        let mut todo = Vec::new();
        for up in upserts {
            match cached.get(&up.path) {
                Some(file) if prefer_cache && file.content_hash == up.hash => {
                    out.insert(up.path.clone(), Arc::clone(file));
                }
                _ => todo.push(up),
            }
        }
        self.fill_texts(conn, ctx, todo.iter().map(|u| u.hash), &mut texts)
            .await?;
        let mut items = Vec::with_capacity(todo.len());
        for up in todo {
            match texts.get(&up.hash) {
                Some(text) => items.push((up.path.clone(), up.hash, Arc::clone(text))),
                None => {
                    return Err(IndexError::Inconsistent(format!(
                        "text of `{}` ({}) is not stored",
                        up.path,
                        up.hash.short()
                    )));
                }
            }
        }
        for file in self
            .analyse_items(ctx, items, purpose.identifiers(), run)
            .await?
        {
            out.insert(file.path.clone(), file);
        }
        Ok(out)
    }

    /// Called when a stage job failed for the last time. A build before
    /// activation is abandoned with the reason, so the view reports it and a
    /// later trigger can start over. A dead T2 runs after activation: the
    /// generation stays active and searchable; its vector index generation
    /// is failed and the tier says why.
    pub(crate) async fn on_dead(&self, job: &Job, error: &str) {
        let Ok(payload) = decode::<StagePayload>(&job.payload) else {
            return;
        };
        let Ok(ctx) = self.context(payload.view) else {
            return;
        };
        let reason = format!("{} failed permanently: {error}", job.kind);
        if job.kind == JOB_EMBEDDINGS {
            let Some(g) = payload.generation else {
                return;
            };
            if let EmbeddingDecision::Embed { profile, .. } = &ctx.embedding
                && let Ok(mut conn) = self.store.acquire().await
            {
                let pin = GenerationPin {
                    view: ctx.view,
                    generation: g,
                };
                if let Ok(Some(ig)) =
                    embeddings::index_generation_at(&mut conn, pin, profile.id).await
                    && ig.state == GenerationState::Building
                    && let Err(e) =
                        embeddings::fail_index_generation(&mut conn, ig.id, &reason).await
                {
                    tracing::warn!(view = %ctx.view, error = %e, "could not record dead embeddings");
                }
            }
            self.cache().forget((ctx.view, g));
            self.set_tier(
                &ctx,
                g,
                &payload.target,
                Tier::T2,
                TierState::Failed { reason },
            );
            return;
        }
        let generation = match payload.generation {
            Some(g) => Some(g),
            None => match self.store.acquire().await {
                Ok(mut conn) => match views::building_generation(&mut conn, ctx.view).await {
                    Ok(Some(g)) => views::get_generation(&mut conn, ctx.view, g)
                        .await
                        .ok()
                        .flatten()
                        .filter(|info| info.resolved_commit.as_deref() == payload.target.commit())
                        .map(|info| info.generation),
                    _ => None,
                },
                Err(_) => None,
            },
        };
        let result = match generation {
            Some(g) => match self.store.acquire().await {
                Ok(mut conn) => views::fail_generation(&mut conn, ctx.view, g, &reason)
                    .await
                    .map_err(IndexError::from),
                Err(e) => Err(e.into()),
            },
            None => self.record_target_failure(&ctx, &reason).await,
        };
        if let Err(e) = result {
            tracing::warn!(view = %ctx.view, error = %e, "could not record a dead build");
        }
        if let (Some(g), Some(tier)) = (generation, crate::jobs::kind_tier(&job.kind)) {
            self.cache().forget((ctx.view, g));
            self.set_tier(
                &ctx,
                g,
                &payload.target,
                tier,
                TierState::Failed {
                    reason: reason.clone(),
                },
            );
        }
        self.record_error(&ctx, generation, &reason);
    }
}
