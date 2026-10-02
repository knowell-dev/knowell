use std::collections::BTreeSet;
use std::time::Duration;

use knowell_store::jobs::*;
use knowell_store::{JobId, JobState, OrganizationId, StoreError, ViewId, WorkspaceId};
use serde_json::json;
use time::OffsetDateTime;

use crate::common::{fixture, require_db};

const LEASE: Duration = Duration::from_secs(30);

fn job(kind: &str, n: u32) -> NewJob {
    NewJob::new(kind, json!({ "n": n }))
}

/// Makes a retry-pending job due now (instead of sleeping through backoff).
async fn make_due(c: &mut sqlx::PgConnection, id: JobId) {
    sqlx::query("UPDATE job SET run_after = now() WHERE id = $1")
        .bind(id)
        .execute(c)
        .await
        .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_claimers_never_share_a_job() {
    let db = require_db!();
    let mut c = db.conn().await;
    let total = 300;
    for n in 0..total {
        enqueue(&mut c, &job("index", n)).await.unwrap();
    }
    let mut workers = Vec::new();
    for w in 0..8 {
        let store = db.store.clone();
        workers.push(tokio::spawn(async move {
            let worker = format!("worker-{w}");
            let mut c = store.acquire().await.unwrap();
            let mut mine = Vec::new();
            while let Some(job) = claim(&mut c, &worker, &["index"], LEASE).await.unwrap() {
                assert_eq!(job.lease_owner.as_deref(), Some(worker.as_str()));
                assert_eq!(job.state, JobState::Running);
                complete(&mut c, job.id, &worker).await.unwrap();
                mine.push(job.id);
            }
            mine
        }));
    }
    let mut all = Vec::new();
    for w in workers {
        all.extend(w.await.unwrap());
    }
    let unique: BTreeSet<JobId> = all.iter().copied().collect();
    assert_eq!(all.len(), total as usize, "every job claimed exactly once");
    assert_eq!(unique.len(), all.len(), "no job claimed twice");
    let counts = job_counts(&mut c).await.unwrap();
    assert_eq!(
        counts,
        vec![JobCount {
            kind: "index".into(),
            state: JobState::Succeeded,
            count: u64::from(total),
        }]
    );
}

#[tokio::test]
async fn enqueue_is_idempotent_by_key() {
    let db = require_db!();
    let mut c = db.conn().await;
    let mut j = job("embed", 1);
    j.idempotency_key = Some("embed:view-1:gen-4".into());
    let first = enqueue(&mut c, &j).await.unwrap();
    assert!(first.created);
    let mut again = job("embed", 2);
    again.idempotency_key = j.idempotency_key.clone();
    let second = enqueue(&mut c, &again).await.unwrap();
    assert_eq!(
        second,
        Enqueued {
            id: first.id,
            created: false
        }
    );
    // The first payload is kept.
    let stored = get_job(&mut c, first.id).await.unwrap().unwrap();
    assert_eq!(stored.payload, json!({ "n": 1 }));

    // Concurrent duplicates also collapse into one job.
    let mut tasks = Vec::new();
    for n in 0..8 {
        let store = db.store.clone();
        tasks.push(tokio::spawn(async move {
            let mut c = store.acquire().await.unwrap();
            let mut j = job("embed", n);
            j.idempotency_key = Some("embed:view-1:gen-5".into());
            enqueue(&mut c, &j).await.unwrap()
        }));
    }
    let mut results = Vec::new();
    for t in tasks {
        results.push(t.await.unwrap());
    }
    assert_eq!(results.iter().filter(|r| r.created).count(), 1);
    assert_eq!(
        results.iter().map(|r| r.id).collect::<BTreeSet<_>>().len(),
        1
    );

    // Invalid jobs are rejected.
    assert!(matches!(
        enqueue(&mut c, &job("", 1)).await,
        Err(StoreError::InvalidInput(_))
    ));
    let mut zero = job("x", 1);
    zero.max_attempts = 0;
    assert!(matches!(
        enqueue(&mut c, &zero).await,
        Err(StoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn claims_follow_priority_then_run_after() {
    let db = require_db!();
    let mut c = db.conn().await;
    let mut ids = Vec::new();
    for (n, priority) in [(0, 0), (1, 10), (2, 5), (3, 10)] {
        let mut j = job("parse", n);
        j.priority = priority;
        ids.push(enqueue(&mut c, &j).await.unwrap().id);
    }
    let mut later = job("parse", 9);
    later.priority = 100;
    later.run_after = Some(OffsetDateTime::now_utc() + time::Duration::hours(1));
    enqueue(&mut c, &later).await.unwrap();
    enqueue(&mut c, &job("other-kind", 0)).await.unwrap();

    let mut order = Vec::new();
    while let Some(j) = claim(&mut c, "w", &["parse"], LEASE).await.unwrap() {
        order.push(j.id);
    }
    // Priority 10 (in run_after order), then 5, then 0; the future job waits.
    assert_eq!(order, vec![ids[1], ids[3], ids[2], ids[0]]);
    assert!(matches!(
        claim(&mut c, "w", &[], LEASE).await,
        Err(StoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn failed_attempts_back_off_exponentially_then_dead_letter() {
    let db = require_db!();
    let mut c = db.conn().await;
    let mut j = job("fetch", 1);
    j.max_attempts = 3;
    let id = enqueue(&mut c, &j).await.unwrap().id;
    let backoff = Backoff {
        base: Duration::from_secs(10),
        max: Duration::from_secs(25),
    };
    let mut delays = Vec::new();
    for attempt in 1..=3u32 {
        let claimed = claim(&mut c, "w", &["fetch"], LEASE)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(claimed.attempts, attempt);
        let outcome = fail(&mut c, id, "w", "upstream returned 503", &backoff)
            .await
            .unwrap();
        let stored = get_job(&mut c, id).await.unwrap().unwrap();
        assert_eq!(stored.last_error.as_deref(), Some("upstream returned 503"));
        assert_eq!(stored.lease_owner, None);
        match outcome {
            FailOutcome::Retrying { run_after } => {
                assert_eq!(stored.state, JobState::Failed);
                delays.push((run_after - stored.updated_at).whole_seconds());
                // Not claimable before its retry time.
                assert!(
                    claim(&mut c, "w", &["fetch"], LEASE)
                        .await
                        .unwrap()
                        .is_none()
                );
                make_due(&mut c, id).await;
            }
            FailOutcome::Dead => {
                assert_eq!(attempt, 3);
                assert_eq!(stored.state, JobState::Dead);
                assert!(stored.finished_at.is_some());
            }
        }
    }
    // 10 s, then 20 s; a third failure is final.
    assert_eq!(delays, vec![10, 20]);
    assert!(
        claim(&mut c, "w", &["fetch"], LEASE)
            .await
            .unwrap()
            .is_none()
    );

    // An operator can requeue a dead job with fresh attempts.
    assert!(requeue_dead(&mut c, id).await.unwrap());
    assert!(!requeue_dead(&mut c, id).await.unwrap());
    let again = claim(&mut c, "w", &["fetch"], LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(again.attempts, 1);

    // The cap applies.
    let backoff = Backoff {
        base: Duration::from_secs(20),
        max: Duration::from_secs(25),
    };
    fail(&mut c, id, "w", "x", &backoff).await.unwrap();
    make_due(&mut c, id).await;
    claim(&mut c, "w", &["fetch"], LEASE)
        .await
        .unwrap()
        .unwrap();
    let FailOutcome::Retrying { run_after } = fail(&mut c, id, "w", "x", &backoff).await.unwrap()
    else {
        panic!("expected a retry");
    };
    let stored = get_job(&mut c, id).await.unwrap().unwrap();
    assert_eq!((run_after - stored.updated_at).whole_seconds(), 25);
}

#[tokio::test]
async fn expired_leases_are_reclaimed_after_a_crash() {
    let db = require_db!();
    let mut c = db.conn().await;
    let mut j = job("index", 1);
    j.max_attempts = 2;
    let id = enqueue(&mut c, &j).await.unwrap().id;
    let crashed = claim(&mut c, "crashed", &["index"], Duration::from_millis(500))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(crashed.id, id);
    // Nothing to reclaim while the lease is valid.
    assert!(reclaim_expired_leases(&mut c).await.unwrap().is_empty());
    tokio::time::sleep(Duration::from_millis(700)).await;
    assert_eq!(reclaim_expired_leases(&mut c).await.unwrap(), vec![id]);
    let stored = get_job(&mut c, id).await.unwrap().unwrap();
    assert_eq!(stored.state, JobState::Queued);
    assert!(stored.last_error.unwrap().contains("lease expired"));

    // The crashed worker has lost the job.
    assert!(matches!(
        heartbeat(&mut c, id, "crashed", LEASE).await,
        Err(StoreError::LeaseLost { .. })
    ));
    assert!(matches!(
        complete(&mut c, id, "crashed").await,
        Err(StoreError::LeaseLost { .. })
    ));

    // A healthy worker takes over and keeps its lease alive.
    let taken = claim(&mut c, "healthy", &["index"], Duration::from_millis(50))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(taken.attempts, 2);
    let expiry = heartbeat(&mut c, id, "healthy", LEASE).await.unwrap();
    assert!(expiry > OffsetDateTime::now_utc() + time::Duration::seconds(20));
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(reclaim_expired_leases(&mut c).await.unwrap().is_empty());

    // Crashing on the last attempt dead-letters the job.
    sqlx::query("UPDATE job SET lease_expires_at = now() - interval '1 second' WHERE id = $1")
        .bind(id)
        .execute(&mut *c)
        .await
        .unwrap();
    assert_eq!(reclaim_expired_leases(&mut c).await.unwrap(), vec![id]);
    assert_eq!(
        get_job(&mut c, id).await.unwrap().unwrap().state,
        JobState::Dead
    );
}

#[tokio::test]
async fn cancelled_jobs_never_run_and_their_workers_stop() {
    let db = require_db!();
    let mut c = db.conn().await;
    let queued = enqueue(&mut c, &job("index", 1)).await.unwrap().id;
    assert!(cancel(&mut c, queued).await.unwrap());
    assert!(!cancel(&mut c, queued).await.unwrap());
    assert!(
        claim(&mut c, "w", &["index"], LEASE)
            .await
            .unwrap()
            .is_none()
    );

    let running = enqueue(&mut c, &job("index", 2)).await.unwrap().id;
    claim(&mut c, "w", &["index"], LEASE)
        .await
        .unwrap()
        .unwrap();
    assert!(cancel(&mut c, running).await.unwrap());
    assert!(matches!(
        heartbeat(&mut c, running, "w", LEASE).await,
        Err(StoreError::LeaseLost { .. })
    ));
    assert!(matches!(
        fail(&mut c, running, "w", "x", &Backoff::default()).await,
        Err(StoreError::LeaseLost { .. })
    ));
    let stored = get_job(&mut c, running).await.unwrap().unwrap();
    assert_eq!(stored.state, JobState::Cancelled);

    // Finished jobs can be purged; their keys become free again.
    let deleted = delete_finished_jobs(
        &mut c,
        OffsetDateTime::now_utc() + time::Duration::seconds(5),
    )
    .await
    .unwrap();
    assert_eq!(deleted, 2);
    assert_eq!(get_job(&mut c, running).await.unwrap(), None);
}

/// Jobs newest first, as `list_jobs` orders them.
fn newest_first(jobs: &[Job]) -> Vec<JobId> {
    let mut sorted: Vec<&Job> = jobs.iter().collect();
    sorted.sort_by_key(|j| std::cmp::Reverse((j.created_at, j.id)));
    sorted.iter().map(|j| j.id).collect()
}

fn ids(jobs: &[Job]) -> BTreeSet<JobId> {
    jobs.iter().map(|j| j.id).collect()
}

#[tokio::test]
async fn listings_filter_by_state_kind_and_tenant_and_page() {
    let db = require_db!();
    let mut c = db.conn().await;
    let a = fixture(&mut c, "a").await;
    let b = fixture(&mut c, "b").await;
    let unscoped = enqueue(&mut c, &job("index.sync", 0)).await.unwrap().id;
    let org_job = enqueue_scoped(
        &mut c,
        &job("source.refresh", 1),
        JobScope::Organization(a.org.id),
    )
    .await
    .unwrap()
    .id;
    let ws_job = enqueue_scoped(
        &mut c,
        &job("view.reindex", 2),
        JobScope::Workspace(a.workspace.id),
    )
    .await
    .unwrap()
    .id;
    let view_job = enqueue_scoped(&mut c, &job("index.text", 3), JobScope::View(a.view.id))
        .await
        .unwrap()
        .id;
    let other = enqueue_scoped(&mut c, &job("index.text", 4), JobScope::View(b.view.id))
        .await
        .unwrap()
        .id;
    let explicit_unscoped = enqueue_scoped(&mut c, &job("gc", 5), JobScope::Unscoped)
        .await
        .unwrap()
        .id;

    // Everything, newest first.
    let all = list_jobs(&mut c, &JobFilter::new(100)).await.unwrap();
    assert_eq!(all.len(), 6);
    assert_eq!(
        all.iter().map(|j| j.id).collect::<Vec<_>>(),
        newest_first(&all)
    );

    let scoped = |scope: JobScopeFilter| JobFilter {
        scope,
        ..JobFilter::new(100)
    };
    let org_a = list_jobs(
        &mut c,
        &scoped(JobScopeFilter::organization(a.org.id, false)),
    )
    .await
    .unwrap();
    assert_eq!(ids(&org_a), BTreeSet::from([org_job, ws_job, view_job]));
    let org_a_and_unscoped = list_jobs(
        &mut c,
        &scoped(JobScopeFilter::organization(a.org.id, true)),
    )
    .await
    .unwrap();
    assert_eq!(
        ids(&org_a_and_unscoped),
        BTreeSet::from([org_job, ws_job, view_job, unscoped, explicit_unscoped])
    );
    let ws_a = list_jobs(
        &mut c,
        &scoped(JobScopeFilter {
            workspace: Some(a.workspace.id),
            ..JobScopeFilter::default()
        }),
    )
    .await
    .unwrap();
    assert_eq!(ids(&ws_a), BTreeSet::from([ws_job, view_job]));
    let org_b = list_jobs(
        &mut c,
        &scoped(JobScopeFilter::organization(b.org.id, false)),
    )
    .await
    .unwrap();
    assert_eq!(ids(&org_b), BTreeSet::from([other]));
    // A workspace of one tenant with the organization of another: nothing.
    let mismatch = list_jobs(
        &mut c,
        &scoped(JobScopeFilter {
            organization: Some(b.org.id),
            workspace: Some(a.workspace.id),
            include_unscoped: false,
        }),
    )
    .await
    .unwrap();
    assert!(mismatch.is_empty());

    // States and kinds.
    let running = claim(&mut c, "w", &["view.reindex"], LEASE)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(running.id, ws_job);
    let only_running = list_jobs(
        &mut c,
        &JobFilter {
            states: vec![JobState::Running],
            ..JobFilter::new(100)
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&only_running), BTreeSet::from([ws_job]));
    let some_kinds = list_jobs(
        &mut c,
        &JobFilter {
            kinds: vec!["index.text".into(), "gc".into()],
            states: vec![JobState::Queued],
            ..JobFilter::new(100)
        },
    )
    .await
    .unwrap();
    assert_eq!(
        ids(&some_kinds),
        BTreeSet::from([view_job, other, explicit_unscoped])
    );

    // Keyset pages cover everything exactly once, in order.
    let mut paged = Vec::new();
    let mut filter = JobFilter::new(4);
    loop {
        let page = list_jobs(&mut c, &filter).await.unwrap();
        assert!(page.len() <= 4);
        let Some(last) = page.last() else { break };
        filter.before = Some(JobCursor::after(last));
        paged.extend(page.iter().map(|j| j.id));
    }
    assert_eq!(paged, all.iter().map(|j| j.id).collect::<Vec<_>>());

    for limit in [0, MAX_JOBS_LISTED + 1] {
        assert!(matches!(
            list_jobs(&mut c, &JobFilter::new(limit)).await,
            Err(StoreError::InvalidInput(_))
        ));
    }
}

#[tokio::test]
async fn scoped_enqueue_checks_its_scope_and_keeps_idempotency() {
    let db = require_db!();
    let mut c = db.conn().await;
    let a = fixture(&mut c, "a").await;
    let missing = uuid::Uuid::now_v7();
    for scope in [
        JobScope::Organization(OrganizationId(missing)),
        JobScope::Workspace(WorkspaceId(missing)),
        JobScope::View(ViewId(missing)),
    ] {
        assert!(
            matches!(
                enqueue_scoped(&mut c, &job("x", 1), scope).await,
                Err(StoreError::NotFound { .. })
            ),
            "{scope:?}"
        );
    }
    assert!(
        list_jobs(&mut c, &JobFilter::new(10))
            .await
            .unwrap()
            .is_empty()
    );

    let mut keyed = job("view.reindex", 1);
    keyed.idempotency_key = Some("view.reindex:user:click-1".into());
    let first = enqueue_scoped(&mut c, &keyed, JobScope::View(a.view.id))
        .await
        .unwrap();
    assert!(first.created);
    // A duplicate key returns the first job, which keeps its scope.
    let again = enqueue(&mut c, &keyed).await.unwrap();
    assert_eq!(again.id, first.id);
    assert!(!again.created);
    let in_workspace = list_jobs(
        &mut c,
        &JobFilter {
            scope: JobScopeFilter {
                workspace: Some(a.workspace.id),
                ..JobScopeFilter::default()
            },
            ..JobFilter::new(10)
        },
    )
    .await
    .unwrap();
    assert_eq!(ids(&in_workspace), BTreeSet::from([first.id]));

    // Deleting the workspace deletes its jobs.
    knowell_store::hierarchy::delete_workspace(&mut c, a.workspace.id)
        .await
        .unwrap();
    assert_eq!(get_job(&mut c, first.id).await.unwrap(), None);
}

#[tokio::test]
async fn oldest_queued_respects_state_and_scope() {
    let db = require_db!();
    let mut c = db.conn().await;
    let a = fixture(&mut c, "a").await;
    let b = fixture(&mut c, "b").await;
    let everything = JobScopeFilter::all();
    assert_eq!(oldest_queued(&mut c, &everything).await.unwrap(), None);

    let first = enqueue(&mut c, &job("unscoped", 1)).await.unwrap().id;
    let a_job = enqueue_scoped(&mut c, &job("a", 2), JobScope::Organization(a.org.id))
        .await
        .unwrap()
        .id;
    enqueue_scoped(&mut c, &job("b", 3), JobScope::View(b.view.id))
        .await
        .unwrap();
    let first_at = get_job(&mut c, first).await.unwrap().unwrap().created_at;
    let a_at = get_job(&mut c, a_job).await.unwrap().unwrap().created_at;
    assert_eq!(
        oldest_queued(&mut c, &everything).await.unwrap(),
        Some(first_at)
    );
    assert_eq!(
        oldest_queued(&mut c, &JobScopeFilter::organization(a.org.id, false))
            .await
            .unwrap(),
        Some(a_at)
    );
    assert_eq!(
        oldest_queued(&mut c, &JobScopeFilter::organization(a.org.id, true))
            .await
            .unwrap(),
        Some(first_at)
    );
    // A running job is no longer waiting.
    claim(&mut c, "w", &["b"], LEASE).await.unwrap().unwrap();
    assert_eq!(
        oldest_queued(&mut c, &JobScopeFilter::organization(b.org.id, false))
            .await
            .unwrap(),
        None
    );
}

#[tokio::test]
async fn scoped_claims_take_only_the_jobs_of_served_views() {
    let db = require_db!();
    let mut c = db.conn().await;
    let a = fixture(&mut c, "a").await;
    let (_, sibling) = crate::common::add_project(&mut c, &a, "a-sibling").await;
    let b = fixture(&mut c, "b").await;
    let kinds = ["index"];

    let mut on_a = job("index", 1);
    on_a.idempotency_key = Some("index:a".into());
    let a_job = enqueue_scoped(&mut c, &on_a, JobScope::View(a.view.id))
        .await
        .unwrap()
        .id;
    let sibling_job = enqueue_scoped(&mut c, &job("index", 2), JobScope::View(sibling.id))
        .await
        .unwrap()
        .id;
    let workspace_job = enqueue_scoped(
        &mut c,
        &job("index", 3),
        JobScope::Workspace(a.workspace.id),
    )
    .await
    .unwrap()
    .id;
    let b_job = enqueue_scoped(&mut c, &job("index", 4), JobScope::View(b.view.id))
        .await
        .unwrap()
        .id;
    let org_job = enqueue_scoped(&mut c, &job("index", 5), JobScope::Organization(a.org.id))
        .await
        .unwrap()
        .id;
    let unscoped_job = enqueue(&mut c, &job("index", 6)).await.unwrap().id;
    assert_eq!(
        find_job_by_key(&mut c, "index:a")
            .await
            .unwrap()
            .map(|j| j.id),
        Some(a_job)
    );
    assert_eq!(find_job_by_key(&mut c, "index:none").await.unwrap(), None);

    // A process serving only view `a` (and its workspace's view-less jobs)
    // never sees the sibling view's or the other tenant's jobs.
    let serving_a = ClaimScope {
        views: vec![a.view.id],
        workspaces: vec![a.workspace.id],
        include_unscoped: false,
    };
    let mut claimed = BTreeSet::new();
    while let Some(j) = claim_scoped(&mut c, "worker-a", &kinds, LEASE, &serving_a)
        .await
        .unwrap()
    {
        claimed.insert(j.id);
        complete(&mut c, j.id, "worker-a").await.unwrap();
    }
    assert_eq!(claimed, BTreeSet::from([a_job, workspace_job]));

    // Unscoped jobs need an explicit opt-in; organization-only jobs and the
    // jobs of unserved views are never claimed by a scoped worker.
    let with_unscoped = ClaimScope {
        include_unscoped: true,
        ..ClaimScope::default()
    };
    let j = claim_scoped(&mut c, "worker-x", &kinds, LEASE, &with_unscoped)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(j.id, unscoped_job);
    complete(&mut c, j.id, "worker-x").await.unwrap();
    assert!(
        claim_scoped(&mut c, "worker-x", &kinds, LEASE, &with_unscoped)
            .await
            .unwrap()
            .is_none()
    );
    let serving_b = ClaimScope {
        views: vec![b.view.id, sibling.id],
        ..ClaimScope::default()
    };
    let mut rest = BTreeSet::new();
    while let Some(j) = claim_scoped(&mut c, "worker-b", &kinds, LEASE, &serving_b)
        .await
        .unwrap()
    {
        rest.insert(j.id);
        complete(&mut c, j.id, "worker-b").await.unwrap();
    }
    assert_eq!(rest, BTreeSet::from([b_job, sibling_job]));
    let left = get_job(&mut c, org_job).await.unwrap().unwrap();
    assert_eq!(left.state, JobState::Queued);
    assert!(matches!(
        claim_scoped(&mut c, "worker-b", &[], LEASE, &serving_b).await,
        Err(StoreError::InvalidInput(_))
    ));

    // Deleting a view deletes its queued jobs.
    let queued = enqueue_scoped(&mut c, &job("index", 7), JobScope::View(b.view.id))
        .await
        .unwrap()
        .id;
    knowell_store::views::delete_view(&mut c, b.view.id)
        .await
        .unwrap();
    assert_eq!(get_job(&mut c, queued).await.unwrap(), None);
}
