//! Change signals and periodic reconciliation.
//!
//! [`crate::Indexer::watch`] starts one `knowell_source` watcher per git
//! working tree of the registered views. Ref and `HEAD` moves sync the
//! views of that repository (queueing builds at [`Priority::Active`]); saved
//! files rebuild the personal overlay of `worktree` views; a rescan does
//! both. Events lost by the platform, changes made while the engine was
//! down and store damage are caught by reconciliation, which runs every
//! [`crate::IndexerConfig::reconcile_interval`]: it re-resolves every
//! target, compares the store's tree hash of the active generation with the
//! one recorded when it was built, restores missing lexical indexes, and
//! queues the embeddings of an active generation that has neither vectors
//! nor a live embedding job (a crash right after activation).

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::mpsc::RecvTimeoutError;
use std::time::Duration;

use knowell_core::TrackTarget;
use knowell_embed::Embedder;
use knowell_source::watch::{SourceEvent, Watcher};
use knowell_store::views::{self, GenerationPin};
use knowell_store::{SourceKind, ViewId};
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

use crate::config::Priority;
use crate::context::ViewContext;
use crate::error::IndexError;
use crate::indexer::{Indexer, Inner, SyncOutcome};
use crate::manifest;
use crate::pipeline::store_tree_hash;
use crate::plan::open_repo;

/// What reconciliation found for one view.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ReconcileReport {
    /// The view.
    pub view: ViewId,
    /// Result of re-resolving its target.
    pub sync: SyncOutcome,
    /// Whether the store's files of the active generation match the tree
    /// hash recorded when it was built (`None` when nothing to compare).
    pub store_consistent: Option<bool>,
    /// Whether a full rebuild was queued because they did not match.
    pub rebuild_queued: bool,
    /// Whether the active generation's lexical index had to be rebuilt.
    pub lexical_restored: bool,
    /// Whether the active generation's embedding stage had to be queued.
    pub embeddings_queued: bool,
}

/// Running watchers and the reconciliation loop; stops on drop.
pub struct WatchHandle {
    cancel: CancellationToken,
    watchers: Vec<Watcher>,
    tasks: Vec<JoinHandle<()>>,
    threads: Vec<std::thread::JoinHandle<()>>,
    watched: Vec<PathBuf>,
}

impl std::fmt::Debug for WatchHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("WatchHandle")
            .field("watched", &self.watched)
            .finish_non_exhaustive()
    }
}

impl WatchHandle {
    /// Working trees being watched.
    pub fn watched(&self) -> &[PathBuf] {
        &self.watched
    }

    /// Stops the watchers and the reconciliation loop.
    pub async fn stop(mut self) {
        self.cancel.cancel();
        for watcher in self.watchers.drain(..) {
            watcher.stop();
        }
        for task in self.tasks.drain(..) {
            let _ = task.await;
        }
        let threads = std::mem::take(&mut self.threads);
        let _ = tokio::task::spawn_blocking(move || {
            for thread in threads {
                let _ = thread.join();
            }
        })
        .await;
    }
}

impl Drop for WatchHandle {
    fn drop(&mut self) {
        self.cancel.cancel();
        for task in &self.tasks {
            task.abort();
        }
    }
}

impl<E: Embedder + 'static> Inner<E> {
    /// Re-resolves every view, checks the store and the lexical index of
    /// the active generations, and queues what is needed.
    pub(crate) async fn reconcile(&self) -> Result<Vec<ReconcileReport>, IndexError> {
        let mut reports = Vec::new();
        for ctx in self.contexts() {
            reports.push(self.reconcile_view(&ctx).await?);
        }
        Ok(reports)
    }

    async fn reconcile_view(&self, ctx: &ViewContext) -> Result<ReconcileReport, IndexError> {
        let sync = self.sync(ctx.view, Priority::Background, false).await?;
        let mut report = ReconcileReport {
            view: ctx.view,
            sync: sync.clone(),
            store_consistent: None,
            rebuild_queued: false,
            lexical_restored: false,
            embeddings_queued: false,
        };
        if !matches!(sync, SyncOutcome::UpToDate { .. }) {
            return Ok(report);
        }
        let mut conn = self.store.acquire().await?;
        let Some(row) = views::get_view(&mut conn, ctx.view).await? else {
            return Ok(report);
        };
        let Some(active) = row.active_generation else {
            return Ok(report);
        };
        let pin = GenerationPin {
            view: ctx.view,
            generation: active,
        };
        if let Some(recorded) = manifest::load(&self.config.data_dir, ctx.view, active) {
            let consistent = store_tree_hash(&mut conn, pin).await? == recorded.tree_hash;
            report.store_consistent = Some(consistent);
            if !consistent {
                tracing::warn!(view = %ctx.view, generation = active, "store files differ from the recorded tree; rebuilding");
                drop(conn);
                let rebuilt = self.sync(ctx.view, Priority::Background, true).await?;
                report.rebuild_queued = matches!(rebuilt, SyncOutcome::Queued { .. });
                return Ok(report);
            }
        }
        if !self.lexical.is_complete(ctx.view, active) {
            self.rebuild_lexical(&mut conn, ctx, pin).await?;
            report.lexical_restored = true;
        }
        drop(conn);
        report.embeddings_queued = self.ensure_embeddings(ctx).await?;
        Ok(report)
    }

    async fn handle_event(&self, views: &[ViewId], event: SourceEvent) {
        let (sync, overlay) = match &event {
            SourceEvent::HeadMoved | SourceEvent::RefsChanged => (true, true),
            SourceEvent::FilesChanged(_) => (false, true),
            SourceEvent::Rescan => (true, true),
        };
        for view in views {
            let Ok(ctx) = self.context(*view) else {
                continue;
            };
            if sync && let Err(error) = self.sync(*view, Priority::Active, false).await {
                tracing::warn!(%view, %error, "sync after a repository event failed");
            }
            if overlay && ctx.target == TrackTarget::WorktreeHead {
                let worktree = ctx.source_path.clone();
                if let Err(error) = self.build_overlay(*view, &worktree).await {
                    tracing::warn!(%view, %error, "overlay rebuild failed");
                }
            }
        }
    }
}

impl<E: Embedder + 'static> Indexer<E> {
    /// Re-resolves every registered view, verifies the store and lexical
    /// index of each active generation, and queues builds or rebuilds where
    /// they are missing (see the module docs of `watch`).
    ///
    /// # Errors
    /// Store errors.
    pub async fn reconcile(&self) -> Result<Vec<ReconcileReport>, IndexError> {
        self.inner.reconcile().await
    }

    /// Starts watching the working trees of the registered git views and
    /// the periodic reconciliation. Must be called inside a tokio runtime.
    /// Builds still need a running [`crate::Worker`] (or
    /// [`Indexer::run_until_idle`]). Plain-directory projects are covered by
    /// reconciliation only.
    ///
    /// # Errors
    /// Git or watcher errors for a working tree that cannot be watched.
    pub fn watch(&self, shutdown: CancellationToken) -> Result<WatchHandle, IndexError> {
        let cancel = shutdown.child_token();
        // One watcher per working tree, shared by the views of its source.
        let mut by_source: BTreeMap<PathBuf, Vec<ViewId>> = BTreeMap::new();
        for ctx in self.inner.contexts() {
            if ctx.source_kind == SourceKind::Git {
                by_source
                    .entry(ctx.source_path.clone())
                    .or_default()
                    .push(ctx.view);
            }
        }
        let (tx, mut rx) = mpsc::unbounded_channel::<(Vec<ViewId>, SourceEvent)>();
        let mut watchers = Vec::new();
        let mut threads = Vec::new();
        let mut watched = Vec::new();
        for (path, views) in by_source {
            let repo = open_repo(&path, self.inner.config.git_config)?;
            if repo.workdir().is_none() {
                continue;
            }
            let policy = match views.first().and_then(|v| self.inner.context(*v).ok()) {
                Some(ctx) => ctx.policy.clone(),
                None => continue,
            };
            let (watcher, events) = Watcher::start(&repo, policy, self.inner.config.watch.clone())?;
            watchers.push(watcher);
            watched.push(path);
            let tx = tx.clone();
            let stop = cancel.clone();
            let thread = std::thread::Builder::new()
                .name("knowell-index-watch".to_owned())
                .spawn(move || {
                    loop {
                        match events.recv_timeout(Duration::from_millis(100)) {
                            Ok(event) => {
                                if tx.send((views.clone(), event)).is_err() {
                                    break;
                                }
                            }
                            Err(RecvTimeoutError::Timeout) => {
                                if stop.is_cancelled() {
                                    break;
                                }
                            }
                            Err(RecvTimeoutError::Disconnected) => break,
                        }
                    }
                })
                .map_err(|e| IndexError::io("starting the watch bridge thread", e))?;
            threads.push(thread);
        }
        drop(tx);
        let mut tasks = Vec::new();
        {
            let inner = Arc::clone(&self.inner);
            let cancel = cancel.clone();
            tasks.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        received = rx.recv() => match received {
                            Some((views, event)) => inner.handle_event(&views, event).await,
                            None => break,
                        },
                    }
                }
            }));
        }
        {
            let inner = Arc::clone(&self.inner);
            let cancel = cancel.clone();
            let period = self
                .inner
                .config
                .reconcile_interval
                .max(Duration::from_secs(1));
            tasks.push(tokio::spawn(async move {
                loop {
                    tokio::select! {
                        () = cancel.cancelled() => break,
                        () = tokio::time::sleep(period) => {
                            if let Err(error) = inner.reconcile().await {
                                tracing::warn!(%error, "reconciliation failed");
                            }
                        }
                    }
                }
            }));
        }
        Ok(WatchHandle {
            cancel,
            watchers,
            tasks,
            threads,
            watched,
        })
    }
}
