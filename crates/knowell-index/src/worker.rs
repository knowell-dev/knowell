//! Running jobs: one job with lease heartbeats and cancellation
//! ([`execute`]), and a long-running [`Worker`] that claims jobs with
//! bounded concurrency until it is shut down.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use knowell_embed::{AnyEmbedder, Embedder};
use knowell_store::StoreError;
use knowell_store::jobs::{self, FailOutcome, Job};
use tokio::task::{JoinHandle, JoinSet};
use tokio_util::sync::CancellationToken;

use crate::config::WorkerConfig;
use crate::error::IndexError;
use crate::indexer::{Counters, Indexer, Inner, JobRun};
use crate::jobs::{JOB_KINDS, StagePayload, decode};
use crate::pipeline::JobOutcome;
use crate::status::ProgressKind;

impl<E: Embedder + 'static> Inner<E> {
    /// Reports a build that ended because a newer one replaced it.
    fn note_superseded(&self, job: &Job, reason: &str) {
        Counters::add(&self.stats.builds_superseded, 1);
        let Ok(payload) = decode::<StagePayload>(&job.payload) else {
            return;
        };
        let Ok(ctx) = self.context(payload.view) else {
            return;
        };
        if let Some(g) = payload.generation {
            self.cache().forget((ctx.view, g));
        }
        tracing::debug!(view = %ctx.view, kind = %job.kind, %reason, "build superseded");
        self.emit(
            &ctx,
            payload.generation,
            Some(&payload.target),
            ProgressKind::Superseded {
                reason: reason.to_owned(),
            },
        );
    }
}

/// Runs one claimed job to its end: heartbeats extend the lease while it
/// runs; a lost lease (the job was cancelled, or the lease expired and
/// another worker took it) or `shutdown` cancels it. Ends the job with
/// `complete` or `fail` (dead-lettering after the last attempt). Returns
/// whether the job succeeded.
pub(crate) async fn execute<E: Embedder + 'static>(
    inner: Arc<Inner<E>>,
    job: Job,
    shutdown: CancellationToken,
) -> bool {
    let cancel = shutdown.child_token();
    let run = JobRun::new(&job, cancel.clone());
    let lease = inner.config.jobs.lease;
    let heartbeat = {
        let inner = Arc::clone(&inner);
        let cancel = cancel.clone();
        let flag = Arc::clone(&run.flag);
        let id = job.id;
        tokio::spawn(async move {
            let interval = lease / 3;
            loop {
                tokio::select! {
                    () = cancel.cancelled() => {
                        flag.store(true, Ordering::Relaxed);
                        break;
                    }
                    () = tokio::time::sleep(interval) => {
                        let beat = match inner.store.acquire().await {
                            Ok(mut conn) => jobs::heartbeat(&mut conn, id, &inner.worker_id, lease)
                                .await
                                .map(|_| ()),
                            Err(e) => Err(e),
                        };
                        match beat {
                            Ok(()) => {}
                            Err(StoreError::LeaseLost { .. }) => {
                                cancel.cancel();
                                flag.store(true, Ordering::Relaxed);
                                break;
                            }
                            Err(error) => {
                                tracing::warn!(job = %id, %error, "lease heartbeat failed");
                            }
                        }
                    }
                }
            }
        })
    };
    let outcome = inner.run_job(&job, &run).await;
    heartbeat.abort();
    Counters::add(&inner.stats.jobs_run, 1);
    let lease_lost = cancel.is_cancelled() && !shutdown.is_cancelled();
    let mut conn = match inner.store.acquire().await {
        Ok(conn) => conn,
        Err(error) => {
            // The lease will expire and the job will be reclaimed.
            tracing::warn!(job = %job.id, %error, "cannot record the job's end");
            return false;
        }
    };
    let worker = inner.worker_id.as_str();
    match outcome {
        Ok(outcome) => {
            if let JobOutcome::Superseded(reason) = &outcome {
                inner.note_superseded(&job, reason);
            }
            match jobs::complete(&mut conn, job.id, worker).await {
                Ok(()) => true,
                Err(error) => {
                    tracing::warn!(job = %job.id, %error, "job finished after losing its lease");
                    false
                }
            }
        }
        // Someone else owns the job now (or it was cancelled): hands off.
        Err(IndexError::Cancelled) if lease_lost => false,
        Err(error) => {
            let text = error.to_string();
            match jobs::fail(&mut conn, job.id, worker, &text, &inner.config.jobs.backoff).await {
                Ok(FailOutcome::Dead) => {
                    tracing::warn!(job = %job.id, kind = %job.kind, error = %text, "job dead-lettered");
                    inner.on_dead(&job, &text).await;
                }
                Ok(FailOutcome::Retrying { .. }) => {
                    tracing::info!(job = %job.id, kind = %job.kind, attempt = job.attempts, error = %text, "job failed; retrying later");
                }
                Err(e) => {
                    tracing::warn!(job = %job.id, error = %e, "cannot record the job failure");
                }
            }
            false
        }
    }
}

/// Claims and runs indexing jobs with bounded concurrency until shut down.
/// Only jobs of the views registered with its indexer are claimed, so
/// several processes can share one queue.
///
/// On start it returns jobs of crashed workers to the queue
/// (`reclaim_expired_leases`), and repeats that once per lease period.
/// Jobs enqueued by the same process wake it immediately; otherwise it
/// polls every [`WorkerConfig::poll_interval`].
pub struct Worker<E: Embedder + 'static = AnyEmbedder> {
    inner: Arc<Inner<E>>,
    config: WorkerConfig,
}

impl<E: Embedder + 'static> std::fmt::Debug for Worker<E> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Worker")
            .field("worker_id", &self.inner.worker_id)
            .field("config", &self.config)
            .finish()
    }
}

impl<E: Embedder + 'static> Worker<E> {
    /// A worker running the jobs of `indexer`.
    pub fn new(indexer: &Indexer<E>, config: WorkerConfig) -> Self {
        Self {
            inner: Arc::clone(&indexer.inner),
            config,
        }
    }

    async fn reclaim(&self) {
        let reclaimed = match self.inner.store.acquire().await {
            Ok(mut conn) => jobs::reclaim_expired_leases(&mut conn).await,
            Err(e) => Err(e),
        };
        match reclaimed {
            Ok(ids) if !ids.is_empty() => {
                tracing::info!(count = ids.len(), "reclaimed jobs of crashed workers");
            }
            Ok(_) => {}
            Err(error) => tracing::warn!(%error, "reclaiming expired leases failed"),
        }
    }

    /// Claims the most urgent job of the views this indexer registered;
    /// other processes' jobs stay queued for them.
    async fn claim(&self) -> Result<Option<Job>, IndexError> {
        let scope = self.inner.claim_scope();
        let mut conn = self.inner.store.acquire().await?;
        Ok(jobs::claim_scoped(
            &mut conn,
            &self.inner.worker_id,
            &JOB_KINDS,
            self.inner.config.jobs.lease,
            &scope,
        )
        .await?)
    }

    /// Runs until `shutdown` is cancelled; running jobs are cancelled too
    /// and retried later.
    ///
    /// # Errors
    /// None today; the result leaves room for fatal conditions.
    pub async fn run(self, shutdown: CancellationToken) -> Result<(), IndexError> {
        let concurrency = self.config.concurrency.max(1);
        let lease = self.inner.config.jobs.lease;
        self.reclaim().await;
        let mut last_reclaim = Instant::now();
        let mut running: JoinSet<bool> = JoinSet::new();
        loop {
            if shutdown.is_cancelled() {
                break;
            }
            while running.try_join_next().is_some() {}
            if running.len() < concurrency {
                match self.claim().await {
                    Ok(Some(job)) => {
                        running.spawn(execute(Arc::clone(&self.inner), job, shutdown.clone()));
                        continue;
                    }
                    Ok(None) => {}
                    Err(error) => tracing::warn!(%error, "claiming a job failed"),
                }
            }
            tokio::select! {
                () = shutdown.cancelled() => break,
                () = self.inner.notify.notified() => {}
                () = tokio::time::sleep(self.config.poll_interval) => {}
                Some(_) = running.join_next(), if !running.is_empty() => {}
            }
            if last_reclaim.elapsed() >= lease {
                self.reclaim().await;
                last_reclaim = Instant::now();
            }
        }
        while running.join_next().await.is_some() {}
        Ok(())
    }

    /// Runs the worker on the current tokio runtime.
    pub fn spawn(self, shutdown: CancellationToken) -> JoinHandle<Result<(), IndexError>> {
        tokio::spawn(self.run(shutdown))
    }
}
