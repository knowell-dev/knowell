//! Durable job queue on PostgreSQL.
//!
//! - [`enqueue`] is idempotent by key.
//! - [`claim`] takes the most urgent claimable job with
//!   `FOR UPDATE SKIP LOCKED`, so concurrent workers never get the same job,
//!   and leases it to the worker for a limited time.
//! - The worker extends the lease with [`heartbeat`] and ends the attempt
//!   with [`complete`] or [`fail`]. A failed attempt is retried with
//!   exponential backoff until `max_attempts`, then the job is `dead`.
//! - [`reclaim_expired_leases`] returns jobs of crashed workers to the queue.
//! - [`enqueue_scoped`] attributes a job to an organization (and workspace,
//!   and view), so [`list_jobs`] and [`oldest_queued`] can answer per tenant
//!   and [`claim_scoped`] hands a job only to a worker serving its view.
//! - [`find_job_by_key`] looks a job up by its idempotency key.
//!
//! Larger `priority` runs first; ties run in `run_after` order.

use std::time::Duration;

use sqlx::PgConnection;
use time::OffsetDateTime;

use crate::error::{StoreError, Violation, violation};
use crate::ids::{JobId, OrganizationId, ViewId, WorkspaceId};
use crate::types::{JobState, from_i32, to_i32, truncate};

/// Most jobs one [`list_jobs`] call returns.
pub const MAX_JOBS_LISTED: u32 = 10_000;

/// Longest error text kept per job, in bytes.
pub const MAX_JOB_ERROR_LEN: usize = 4096;
/// Longest lease a worker can take.
pub const MAX_LEASE: Duration = Duration::from_secs(24 * 3600);
const MAX_WORKER_LEN: usize = 200;

/// A job to enqueue.
#[derive(Debug, Clone, PartialEq)]
pub struct NewJob {
    /// What to do (`index_view`, `embed_chunks`, ...); workers claim by kind.
    pub kind: String,
    /// Arguments. Must not contain secret values.
    pub payload: serde_json::Value,
    /// Larger runs first.
    pub priority: i32,
    /// Attempts before the job is dead-lettered (at least 1).
    pub max_attempts: u32,
    /// Do not run before this time (`None` = now).
    pub run_after: Option<OffsetDateTime>,
    /// Deduplicates enqueues: a second job with the same key is not
    /// created; the first one's id is returned instead. Keys stay taken
    /// until the job record is deleted.
    pub idempotency_key: Option<String>,
}

impl NewJob {
    /// A job of `kind` with priority 0, 5 attempts, runnable now, no key.
    pub fn new(kind: impl Into<String>, payload: serde_json::Value) -> Self {
        Self {
            kind: kind.into(),
            payload,
            priority: 0,
            max_attempts: 5,
            run_after: None,
            idempotency_key: None,
        }
    }
}

/// A stored job.
#[derive(Debug, Clone, PartialEq)]
pub struct Job {
    /// Id.
    pub id: JobId,
    /// Kind.
    pub kind: String,
    /// Arguments.
    pub payload: serde_json::Value,
    /// Priority (larger first).
    pub priority: i32,
    /// State.
    pub state: JobState,
    /// Attempts started so far (incremented when claimed).
    pub attempts: u32,
    /// Attempts allowed.
    pub max_attempts: u32,
    /// Earliest time of the next run.
    pub run_after: OffsetDateTime,
    /// Worker holding the lease, while running.
    pub lease_owner: Option<String>,
    /// When the lease expires, while running.
    pub lease_expires_at: Option<OffsetDateTime>,
    /// Deduplication key.
    pub idempotency_key: Option<String>,
    /// Error of the last failed attempt.
    pub last_error: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last state change.
    pub updated_at: OffsetDateTime,
    /// Start of the latest attempt.
    pub started_at: Option<OffsetDateTime>,
    /// When the job reached a final state.
    pub finished_at: Option<OffsetDateTime>,
}

/// Result of [`enqueue`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Enqueued {
    /// The job (new, or the existing one with the same idempotency key).
    pub id: JobId,
    /// Whether this call created it.
    pub created: bool,
}

/// Retry delays: `base * 2^(attempt - 1)`, capped at `max`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Backoff {
    /// Delay after the first failed attempt.
    pub base: Duration,
    /// Longest delay.
    pub max: Duration,
}

impl Default for Backoff {
    fn default() -> Self {
        Self {
            base: Duration::from_secs(5),
            max: Duration::from_secs(3600),
        }
    }
}

/// What happened to a job after [`fail`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FailOutcome {
    /// It will be retried (state `failed`, claimable again after `run_after`).
    Retrying {
        /// Earliest time of the retry.
        run_after: OffsetDateTime,
    },
    /// It used up its attempts (state `dead`).
    Dead,
}

/// Number of jobs of one kind in one state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobCount {
    /// Kind.
    pub kind: String,
    /// State.
    pub state: JobState,
    /// Count.
    pub count: u64,
}

#[derive(sqlx::FromRow)]
struct JobRow {
    id: JobId,
    kind: String,
    payload: serde_json::Value,
    priority: i32,
    state: JobState,
    attempts: i32,
    max_attempts: i32,
    run_after: OffsetDateTime,
    lease_owner: Option<String>,
    lease_expires_at: Option<OffsetDateTime>,
    idempotency_key: Option<String>,
    last_error: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
    started_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

impl TryFrom<JobRow> for Job {
    type Error = StoreError;

    fn try_from(row: JobRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            kind: row.kind,
            payload: row.payload,
            priority: row.priority,
            state: row.state,
            attempts: from_i32(row.attempts, "attempts")?,
            max_attempts: from_i32(row.max_attempts, "max attempts")?,
            run_after: row.run_after,
            lease_owner: row.lease_owner,
            lease_expires_at: row.lease_expires_at,
            idempotency_key: row.idempotency_key,
            last_error: row.last_error,
            created_at: row.created_at,
            updated_at: row.updated_at,
            started_at: row.started_at,
            finished_at: row.finished_at,
        })
    }
}

macro_rules! job_columns {
    () => {
        "id, kind, payload, priority, state, attempts, max_attempts, run_after, lease_owner, lease_expires_at, idempotency_key, last_error, created_at, updated_at, started_at, finished_at"
    };
}

fn millis(duration: Duration, what: &str) -> Result<i64, StoreError> {
    i64::try_from(duration.as_millis())
        .map_err(|_| StoreError::invalid(format!("{what} is too long")))
}

fn check_worker(worker: &str) -> Result<(), StoreError> {
    if worker.is_empty() || worker.len() > MAX_WORKER_LEN {
        return Err(StoreError::invalid(format!(
            "worker name must be 1-{MAX_WORKER_LEN} bytes"
        )));
    }
    Ok(())
}

fn check_lease(lease: Duration) -> Result<i64, StoreError> {
    if lease.is_zero() || lease > MAX_LEASE {
        return Err(StoreError::invalid(
            "lease must be between 1 ms and 24 hours",
        ));
    }
    millis(lease, "lease")
}

/// Adds a job. With an idempotency key that is already taken, nothing is
/// created and the existing job's id is returned with `created: false`.
///
/// The job is unscoped (attributed to no organization); prefer
/// [`enqueue_scoped`] when the tenant is known.
pub async fn enqueue(conn: &mut PgConnection, job: &NewJob) -> Result<Enqueued, StoreError> {
    insert_job(conn, job, None, None, None).await
}

/// Which tenant a job belongs to, for [`enqueue_scoped`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum JobScope {
    /// Attributed to no organization (system-wide work, or a producer that
    /// cannot tell).
    Unscoped,
    /// An organization as a whole.
    Organization(OrganizationId),
    /// A workspace (its organization is looked up).
    Workspace(WorkspaceId),
    /// The workspace and organization of a view's project.
    View(ViewId),
}

/// Adds a job attributed to `scope`. Otherwise exactly like [`enqueue`]; when
/// the idempotency key is already taken, the existing job keeps its own
/// scope. Fails with [`StoreError::NotFound`] when the organization,
/// workspace or view does not exist.
///
/// A [`JobScope::View`] job also records the view itself (migration 0010),
/// so [`claim_scoped`] hands it only to workers serving that view.
pub async fn enqueue_scoped(
    conn: &mut PgConnection,
    job: &NewJob,
    scope: JobScope,
) -> Result<Enqueued, StoreError> {
    let mut view_id = None;
    let (organization, workspace) = match scope {
        JobScope::Unscoped => (None, None),
        JobScope::Organization(org) => (Some(org), None),
        JobScope::Workspace(ws) => {
            let org: Option<OrganizationId> =
                sqlx::query_scalar("SELECT organization_id FROM workspace WHERE id = $1")
                    .bind(ws)
                    .fetch_optional(&mut *conn)
                    .await?;
            let org = org.ok_or_else(|| StoreError::not_found("workspace", ws))?;
            (Some(org), Some(ws))
        }
        JobScope::View(view) => {
            let row: Option<(OrganizationId, WorkspaceId)> = sqlx::query_as(
                "SELECT p.organization_id, p.workspace_id
                 FROM view v JOIN project p ON p.id = v.project_id
                 WHERE v.id = $1",
            )
            .bind(view)
            .fetch_optional(&mut *conn)
            .await?;
            let (org, ws) = row.ok_or_else(|| StoreError::not_found("view", view))?;
            view_id = Some(view);
            (Some(org), Some(ws))
        }
    };
    insert_job(conn, job, organization, workspace, view_id)
        .await
        .map_err(|err| match err {
            StoreError::Database(e) if matches!(violation(&e), Some(Violation::ForeignKey(_))) => {
                match (organization, workspace, view_id) {
                    (_, _, Some(view)) => StoreError::not_found("view", view),
                    (_, Some(ws), None) => StoreError::not_found("workspace", ws),
                    (Some(org), None, None) => StoreError::not_found("organization", org),
                    (None, None, None) => StoreError::Database(e),
                }
            }
            other => other,
        })
}

async fn insert_job(
    conn: &mut PgConnection,
    job: &NewJob,
    organization: Option<OrganizationId>,
    workspace: Option<WorkspaceId>,
    view: Option<ViewId>,
) -> Result<Enqueued, StoreError> {
    if job.kind.is_empty() {
        return Err(StoreError::invalid("job kind must not be empty"));
    }
    if job.max_attempts == 0 {
        return Err(StoreError::invalid("max_attempts must be at least 1"));
    }
    if job.idempotency_key.as_deref() == Some("") {
        return Err(StoreError::invalid("idempotency key must not be empty"));
    }
    let max_attempts = to_i32(job.max_attempts, "max attempts")?;
    // Retried because a conflicting insert can commit after our statement's
    // snapshot (then the lookup misses it) or roll back (then we can insert).
    for _ in 0..3 {
        let inserted: Option<JobId> = sqlx::query_scalar(
            "INSERT INTO job (kind, payload, priority, max_attempts, run_after, idempotency_key,
                              organization_id, workspace_id, view_id)
             VALUES ($1, $2, $3, $4, coalesce($5, now()), $6, $7, $8, $9)
             ON CONFLICT (idempotency_key) DO NOTHING
             RETURNING id",
        )
        .bind(&job.kind)
        .bind(&job.payload)
        .bind(job.priority)
        .bind(max_attempts)
        .bind(job.run_after)
        .bind(job.idempotency_key.as_deref())
        .bind(organization)
        .bind(workspace)
        .bind(view)
        .fetch_optional(&mut *conn)
        .await?;
        if let Some(id) = inserted {
            return Ok(Enqueued { id, created: true });
        }
        let existing: Option<JobId> =
            sqlx::query_scalar("SELECT id FROM job WHERE idempotency_key = $1")
                .bind(job.idempotency_key.as_deref())
                .fetch_optional(&mut *conn)
                .await?;
        if let Some(id) = existing {
            return Ok(Enqueued { id, created: false });
        }
    }
    Err(StoreError::Corrupt(
        "job with this idempotency key neither inserted nor found; retry".to_owned(),
    ))
}

/// Looks a job up.
pub async fn get_job(conn: &mut PgConnection, id: JobId) -> Result<Option<Job>, StoreError> {
    let row = sqlx::query_as::<_, JobRow>(concat!(
        "SELECT ",
        job_columns!(),
        " FROM job WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Claims the most urgent runnable job of one of `kinds` (queued, or failed
/// and due for retry) for `worker`, leased for `lease`. Returns `None` when
/// nothing is runnable. Concurrent claimers never receive the same job.
pub async fn claim(
    conn: &mut PgConnection,
    worker: &str,
    kinds: &[&str],
    lease: Duration,
) -> Result<Option<Job>, StoreError> {
    check_worker(worker)?;
    if kinds.is_empty() {
        return Err(StoreError::invalid("claim needs at least one job kind"));
    }
    let lease_ms = check_lease(lease)?;
    let row = sqlx::query_as::<_, JobRow>(concat!(
        "UPDATE job j
         SET state = 'running', lease_owner = $1,
             lease_expires_at = now() + $3 * interval '1 millisecond',
             attempts = j.attempts + 1, started_at = now(), updated_at = now()
         FROM (
           SELECT id FROM job
           WHERE state IN ('queued', 'failed') AND run_after <= now() AND kind = ANY($2)
           ORDER BY priority DESC, run_after, id
           LIMIT 1
           FOR UPDATE SKIP LOCKED
         ) next
         WHERE j.id = next.id
         RETURNING ",
        "j.id, j.kind, j.payload, j.priority, j.state, j.attempts, j.max_attempts, j.run_after,
         j.lease_owner, j.lease_expires_at, j.idempotency_key, j.last_error, j.created_at,
         j.updated_at, j.started_at, j.finished_at"
    ))
    .bind(worker)
    .bind(kinds)
    .bind(lease_ms)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Which jobs [`claim_scoped`] may hand out: those of the views and
/// workspaces a worker serves.
///
/// A job matches when it was enqueued for one of `views`
/// ([`JobScope::View`]), or carries no view and belongs to one of
/// `workspaces` ([`JobScope::Workspace`]), or is unscoped (no organization)
/// and `include_unscoped` is set. Organization-only jobs never match: no
/// worker can tell whether it serves them. A view-scoped job of a listed
/// workspace does *not* match unless its view is listed too, so processes
/// serving different views of one workspace never take each other's jobs.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ClaimScope {
    /// Views whose jobs may be claimed.
    pub views: Vec<ViewId>,
    /// Workspaces whose view-less jobs may be claimed.
    pub workspaces: Vec<WorkspaceId>,
    /// Also claim jobs attributed to no organization (enqueued with
    /// [`enqueue`] or [`JobScope::Unscoped`]).
    pub include_unscoped: bool,
}

/// Like [`claim`], restricted to the jobs `scope` selects, so several
/// processes sharing one queue, each serving its own views, never claim (and
/// then fail and dead-letter) each other's jobs. Jobs outside the scope stay
/// queued for the worker that serves them.
pub async fn claim_scoped(
    conn: &mut PgConnection,
    worker: &str,
    kinds: &[&str],
    lease: Duration,
    scope: &ClaimScope,
) -> Result<Option<Job>, StoreError> {
    check_worker(worker)?;
    if kinds.is_empty() {
        return Err(StoreError::invalid("claim needs at least one job kind"));
    }
    let lease_ms = check_lease(lease)?;
    let row = sqlx::query_as::<_, JobRow>(concat!(
        "UPDATE job j
         SET state = 'running', lease_owner = $1,
             lease_expires_at = now() + $3 * interval '1 millisecond',
             attempts = j.attempts + 1, started_at = now(), updated_at = now()
         FROM (
           SELECT id FROM job
           WHERE state IN ('queued', 'failed') AND run_after <= now() AND kind = ANY($2)
             AND (view_id = ANY($4::uuid[])
                  OR (view_id IS NULL AND workspace_id = ANY($5::uuid[]))
                  OR (organization_id IS NULL AND $6::boolean))
           ORDER BY priority DESC, run_after, id
           LIMIT 1
           FOR UPDATE SKIP LOCKED
         ) next
         WHERE j.id = next.id
         RETURNING ",
        "j.id, j.kind, j.payload, j.priority, j.state, j.attempts, j.max_attempts, j.run_after,
         j.lease_owner, j.lease_expires_at, j.idempotency_key, j.last_error, j.created_at,
         j.updated_at, j.started_at, j.finished_at"
    ))
    .bind(worker)
    .bind(kinds)
    .bind(lease_ms)
    .bind(&scope.views)
    .bind(&scope.workspaces)
    .bind(scope.include_unscoped)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// The job that holds `key` as its idempotency key, in whatever state.
pub async fn find_job_by_key(
    conn: &mut PgConnection,
    key: &str,
) -> Result<Option<Job>, StoreError> {
    let row = sqlx::query_as::<_, JobRow>(concat!(
        "SELECT ",
        job_columns!(),
        " FROM job WHERE idempotency_key = $1"
    ))
    .bind(key)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Extends the lease of a running job held by `worker` to `lease` from now
/// and returns the new expiry. Fails with [`StoreError::LeaseLost`] if the
/// worker no longer holds it (the worker must stop working on the job).
pub async fn heartbeat(
    conn: &mut PgConnection,
    id: JobId,
    worker: &str,
    lease: Duration,
) -> Result<OffsetDateTime, StoreError> {
    check_worker(worker)?;
    let lease_ms = check_lease(lease)?;
    let expires: Option<OffsetDateTime> = sqlx::query_scalar(
        "UPDATE job SET lease_expires_at = now() + $3 * interval '1 millisecond', updated_at = now()
         WHERE id = $1 AND state = 'running' AND lease_owner = $2
         RETURNING lease_expires_at",
    )
    .bind(id)
    .bind(worker)
    .bind(lease_ms)
    .fetch_optional(conn)
    .await?;
    expires.ok_or_else(|| StoreError::LeaseLost {
        job: id,
        worker: worker.to_owned(),
    })
}

/// Marks a running job held by `worker` as succeeded.
pub async fn complete(conn: &mut PgConnection, id: JobId, worker: &str) -> Result<(), StoreError> {
    check_worker(worker)?;
    let done = sqlx::query(
        "UPDATE job SET state = 'succeeded', lease_owner = NULL, lease_expires_at = NULL,
                        updated_at = now(), finished_at = now()
         WHERE id = $1 AND state = 'running' AND lease_owner = $2",
    )
    .bind(id)
    .bind(worker)
    .execute(conn)
    .await?;
    if done.rows_affected() == 0 {
        return Err(StoreError::LeaseLost {
            job: id,
            worker: worker.to_owned(),
        });
    }
    Ok(())
}

/// Ends a failed attempt of a running job held by `worker`. The job is
/// retried after `backoff` (state `failed`) unless it has used all its
/// attempts (state `dead`). `error` is truncated to [`MAX_JOB_ERROR_LEN`]
/// and must not contain secrets.
pub async fn fail(
    conn: &mut PgConnection,
    id: JobId,
    worker: &str,
    error: &str,
    backoff: &Backoff,
) -> Result<FailOutcome, StoreError> {
    check_worker(worker)?;
    let base = millis(backoff.base, "backoff")?;
    let max = millis(backoff.max, "backoff")?;
    let row: Option<(JobState, OffsetDateTime)> = sqlx::query_as(
        "UPDATE job SET
           state = CASE WHEN attempts >= max_attempts THEN 'dead'::job_state
                        ELSE 'failed'::job_state END,
           run_after = CASE WHEN attempts >= max_attempts THEN run_after
                            ELSE now() + least($4::double precision
                                               * power(2::double precision, greatest(attempts - 1, 0)),
                                               $5::double precision)
                                         * interval '1 millisecond' END,
           finished_at = CASE WHEN attempts >= max_attempts THEN now() ELSE NULL END,
           last_error = $3, lease_owner = NULL, lease_expires_at = NULL, updated_at = now()
         WHERE id = $1 AND state = 'running' AND lease_owner = $2
         RETURNING state, run_after",
    )
    .bind(id)
    .bind(worker)
    .bind(truncate(error, MAX_JOB_ERROR_LEN))
    .bind(base)
    .bind(max)
    .fetch_optional(conn)
    .await?;
    match row {
        None => Err(StoreError::LeaseLost {
            job: id,
            worker: worker.to_owned(),
        }),
        Some((JobState::Dead, _)) => Ok(FailOutcome::Dead),
        Some((_, run_after)) => Ok(FailOutcome::Retrying { run_after }),
    }
}

/// Cancels a job that has not finished. A running job's worker learns it at
/// its next heartbeat ([`StoreError::LeaseLost`]). Returns whether the job
/// was cancelled by this call.
pub async fn cancel(conn: &mut PgConnection, id: JobId) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE job SET state = 'cancelled', lease_owner = NULL, lease_expires_at = NULL,
                        updated_at = now(), finished_at = now()
         WHERE id = $1 AND state IN ('queued', 'running', 'failed')",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Crash recovery: running jobs whose lease expired go back to the queue
/// (or to `dead` if that was their last attempt). Returns their ids.
pub async fn reclaim_expired_leases(conn: &mut PgConnection) -> Result<Vec<JobId>, StoreError> {
    reclaim_expired_leases_matching(conn, None).await
}

/// Crash recovery restricted to the view, workspace and unscoped jobs selected
/// by `scope`, using the same matching rules as [`claim_scoped`]. Jobs outside
/// the scope keep their state, attempts and lease, even when it has expired.
pub async fn reclaim_expired_leases_scoped(
    conn: &mut PgConnection,
    scope: &ClaimScope,
) -> Result<Vec<JobId>, StoreError> {
    reclaim_expired_leases_matching(conn, Some(scope)).await
}

async fn reclaim_expired_leases_matching(
    conn: &mut PgConnection,
    scope: Option<&ClaimScope>,
) -> Result<Vec<JobId>, StoreError> {
    let include_all = scope.is_none();
    let empty = ClaimScope::default();
    let scope = scope.unwrap_or(&empty);
    let mut ids: Vec<JobId> = sqlx::query_scalar(
        "UPDATE job SET
           state = CASE WHEN attempts >= max_attempts THEN 'dead'::job_state
                        ELSE 'queued'::job_state END,
           finished_at = CASE WHEN attempts >= max_attempts THEN now() ELSE NULL END,
           last_error = 'lease expired before the worker finished',
           lease_owner = NULL, lease_expires_at = NULL, updated_at = now()
         WHERE id IN (SELECT id FROM job
                      WHERE state = 'running' AND lease_expires_at < now()
                        AND ($1::boolean
                             OR view_id = ANY($2::uuid[])
                             OR (view_id IS NULL AND workspace_id = ANY($3::uuid[]))
                             OR (organization_id IS NULL AND $4::boolean))
                      FOR UPDATE SKIP LOCKED)
         RETURNING id",
    )
    .bind(include_all)
    .bind(&scope.views)
    .bind(&scope.workspaces)
    .bind(scope.include_unscoped)
    .fetch_all(conn)
    .await?;
    ids.sort();
    Ok(ids)
}

/// Puts a dead job back into the queue with a fresh set of attempts.
/// Returns whether it was dead.
pub async fn requeue_dead(conn: &mut PgConnection, id: JobId) -> Result<bool, StoreError> {
    let done = sqlx::query(
        "UPDATE job SET state = 'queued', attempts = 0, run_after = now(), finished_at = NULL,
                        updated_at = now()
         WHERE id = $1 AND state = 'dead'",
    )
    .bind(id)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Deletes succeeded and cancelled jobs that finished before `before`
/// (their idempotency keys become free again). Returns how many.
pub async fn delete_finished_jobs(
    conn: &mut PgConnection,
    before: OffsetDateTime,
) -> Result<u64, StoreError> {
    let done = sqlx::query(
        "DELETE FROM job WHERE state IN ('succeeded', 'cancelled') AND finished_at < $1",
    )
    .bind(before)
    .execute(conn)
    .await?;
    Ok(done.rows_affected())
}

/// Job counts per kind and state, ordered by kind then state.
pub async fn job_counts(conn: &mut PgConnection) -> Result<Vec<JobCount>, StoreError> {
    let rows: Vec<(String, JobState, i64)> = sqlx::query_as(
        "SELECT kind, state, count(*) FROM job GROUP BY kind, state ORDER BY kind COLLATE \"C\", state",
    )
    .fetch_all(conn)
    .await?;
    rows.into_iter()
        .map(|(kind, state, count)| {
            Ok(JobCount {
                kind,
                state,
                count: u64::try_from(count)
                    .map_err(|_| StoreError::Corrupt("negative job count".to_owned()))?,
            })
        })
        .collect()
}

/// Which tenants' jobs a listing covers.
///
/// With neither `organization` nor `workspace` set, every job matches (the
/// whole queue). Otherwise a job matches when it carries the given
/// organization and/or workspace; unscoped jobs (enqueued with
/// [`enqueue`] or [`JobScope::Unscoped`]) match only with
/// `include_unscoped`.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct JobScopeFilter {
    /// Jobs of this organization (any of its workspaces, or none).
    pub organization: Option<OrganizationId>,
    /// Jobs of this workspace.
    pub workspace: Option<WorkspaceId>,
    /// Also include jobs attributed to no organization.
    pub include_unscoped: bool,
}

impl JobScopeFilter {
    /// Every job, scoped or not.
    pub fn all() -> Self {
        Self::default()
    }

    /// Jobs of one organization, optionally with the unscoped ones.
    pub fn organization(organization: OrganizationId, include_unscoped: bool) -> Self {
        Self {
            organization: Some(organization),
            workspace: None,
            include_unscoped,
        }
    }
}

/// Keyset position for paging through [`list_jobs`]: the listing continues
/// with jobs strictly older than this one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct JobCursor {
    /// Creation time of the last job seen.
    pub created_at: OffsetDateTime,
    /// Id of the last job seen (tie-break).
    pub id: JobId,
}

impl JobCursor {
    /// The cursor after `job` (pass the last job of a page).
    pub fn after(job: &Job) -> Self {
        Self {
            created_at: job.created_at,
            id: job.id,
        }
    }
}

/// What [`list_jobs`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JobFilter {
    /// Jobs in any of these states; empty = every state.
    pub states: Vec<JobState>,
    /// Jobs of any of these kinds; empty = every kind.
    pub kinds: Vec<String>,
    /// Tenant restriction.
    pub scope: JobScopeFilter,
    /// Most jobs to return, 1..=[`MAX_JOBS_LISTED`].
    pub limit: u32,
    /// Continue after this position (newest-first order).
    pub before: Option<JobCursor>,
}

impl JobFilter {
    /// Every job, newest first, at most `limit`.
    pub fn new(limit: u32) -> Self {
        Self {
            states: Vec::new(),
            kinds: Vec::new(),
            scope: JobScopeFilter::all(),
            limit,
            before: None,
        }
    }
}

/// SQL condition for a [`JobScopeFilter`] whose parameters are `$org`,
/// `$ws` and `$unscoped` at the given positions.
macro_rules! scope_condition {
    ($org:literal, $ws:literal, $unscoped:literal) => {
        concat!(
            "(($",
            $org,
            "::uuid IS NULL AND $",
            $ws,
            "::uuid IS NULL)
              OR (organization_id IS NULL AND $",
            $unscoped,
            "::boolean)
              OR (organization_id IS NOT NULL
                  AND ($",
            $org,
            "::uuid IS NULL OR organization_id = $",
            $org,
            ")
                  AND ($",
            $ws,
            "::uuid IS NULL OR workspace_id = $",
            $ws,
            ")))"
        )
    };
}

/// Jobs matching `filter`, newest first (`created_at` descending, ties by id
/// descending). Page with [`JobFilter::before`] = [`JobCursor::after`] of
/// the last job returned.
pub async fn list_jobs(
    conn: &mut PgConnection,
    filter: &JobFilter,
) -> Result<Vec<Job>, StoreError> {
    if filter.limit == 0 || filter.limit > MAX_JOBS_LISTED {
        return Err(StoreError::invalid(format!(
            "job listing limit must be between 1 and {MAX_JOBS_LISTED}"
        )));
    }
    let scope = &filter.scope;
    let rows = sqlx::query_as::<_, JobRow>(concat!(
        "SELECT ",
        job_columns!(),
        " FROM job
         WHERE (cardinality($1::job_state[]) = 0 OR state = ANY($1))
           AND (cardinality($2::text[]) = 0 OR kind = ANY($2))
           AND ",
        scope_condition!("3", "4", "5"),
        "
           AND ($6::timestamptz IS NULL OR (created_at, id) < ($6, $7::uuid))
         ORDER BY created_at DESC, id DESC
         LIMIT $8"
    ))
    .bind(&filter.states)
    .bind(&filter.kinds)
    .bind(scope.organization)
    .bind(scope.workspace)
    .bind(scope.include_unscoped)
    .bind(filter.before.map(|c| c.created_at))
    .bind(filter.before.map(|c| c.id))
    .bind(i64::from(filter.limit))
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Creation time of the oldest job in state `queued` within `scope`, if any
/// (retry-pending `failed` jobs are not counted).
pub async fn oldest_queued(
    conn: &mut PgConnection,
    scope: &JobScopeFilter,
) -> Result<Option<OffsetDateTime>, StoreError> {
    let oldest: Option<OffsetDateTime> = sqlx::query_scalar(concat!(
        "SELECT min(created_at) FROM job WHERE state = 'queued' AND ",
        scope_condition!("1", "2", "3")
    ))
    .bind(scope.organization)
    .bind(scope.workspace)
    .bind(scope.include_unscoped)
    .fetch_one(conn)
    .await?;
    Ok(oldest)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_condition_names_its_parameters() {
        let sql = scope_condition!("3", "4", "5");
        assert!(sql.contains("$3::uuid IS NULL AND $4::uuid IS NULL"));
        assert!(sql.contains("organization_id IS NULL AND $5::boolean"));
        assert!(sql.contains("workspace_id = $4"));
        assert!(!sql.contains("$1") && !sql.contains("$6"));
    }

    #[test]
    fn filters_default_to_everything() {
        let filter = JobFilter::new(10);
        assert_eq!(filter.scope, JobScopeFilter::all());
        assert!(filter.states.is_empty() && filter.kinds.is_empty());
        let org = OrganizationId(uuid::Uuid::nil());
        let scoped = JobScopeFilter::organization(org, true);
        assert_eq!(scoped.organization, Some(org));
        assert!(scoped.include_unscoped && scoped.workspace.is_none());
    }

    #[test]
    fn leases_are_bounded() {
        assert!(check_lease(Duration::ZERO).is_err());
        assert!(check_lease(MAX_LEASE + Duration::from_secs(1)).is_err());
        assert_eq!(check_lease(Duration::from_secs(2)).unwrap(), 2000);
    }

    #[test]
    fn workers_are_named() {
        assert!(check_worker("").is_err());
        assert!(check_worker(&"w".repeat(MAX_WORKER_LEN + 1)).is_err());
        assert!(check_worker("worker-1").is_ok());
    }

    #[test]
    fn new_job_defaults() {
        let job = NewJob::new("index_view", serde_json::json!({"view": 1}));
        assert_eq!(job.priority, 0);
        assert_eq!(job.max_attempts, 5);
        assert!(job.idempotency_key.is_none());
    }
}
