//! `/api/v1/health` and `/api/v1/health/live`.

use axum::Json;
use axum::extract::State;
use knowell_store::jobs::{self, JobScopeFilter};
use knowell_store::{JobState, Store};
use time::OffsetDateTime;

use crate::engine::{EngineError, EngineRequest};
use crate::error::ApiError;
use crate::extract::Caller;
use crate::state::AppState;
use crate::wire::{ComponentHealth, EngineHealth, HealthStatus, Liveness, QueueStats};

/// `GET /api/v1/health/live`: unauthenticated liveness for process
/// supervisors. Reveals nothing but that the process answers.
pub(super) async fn live() -> Json<Liveness> {
    Json(Liveness { status: "ok" })
}

/// `GET /api/v1/health`: engine health for any authenticated caller.
pub(super) async fn health(
    State(state): State<AppState>,
    caller: Caller,
) -> Result<Json<EngineHealth>, ApiError> {
    let inner = state.inner();
    let mut components = Vec::new();

    let queue = match &inner.store {
        None => {
            components.push(component(
                "database",
                HealthStatus::Down,
                "no database is configured for this server",
            ));
            None
        }
        Some(store) => {
            let (db, queue) = database(store).await;
            components.push(db);
            queue
        }
    };

    let mut freshness = None;
    let mut recent_errors = None;
    let mut resources = None;
    match &inner.engine {
        None => components.push(component(
            "engine",
            HealthStatus::Down,
            &inner.engine_reason,
        )),
        Some(engine) => {
            let ctx = caller.engine_context(&state);
            match engine.call(&ctx, EngineRequest::HealthDetail).await {
                Ok(serde_json::Value::Object(mut detail)) => {
                    freshness = detail.remove("freshness");
                    recent_errors = detail.remove("recentErrors");
                    resources = detail.remove("resources");
                    components.push(component("engine", HealthStatus::Ok, "engine is answering"));
                }
                Ok(_) => components.push(component(
                    "engine",
                    HealthStatus::Degraded,
                    "the engine returned a malformed health report",
                )),
                Err(EngineError::Unavailable { reason }) => {
                    components.push(component("engine", HealthStatus::Degraded, &reason));
                }
                Err(err) => {
                    tracing::warn!(error = %err, "engine health check failed");
                    components.push(component(
                        "engine",
                        HealthStatus::Degraded,
                        "the engine health check failed; see the server log",
                    ));
                }
            }
        }
    }

    let (panel_ok, panel_detail) = inner.panel.describe();
    components.push(component(
        "panel",
        if panel_ok {
            HealthStatus::Ok
        } else {
            HealthStatus::Degraded
        },
        panel_detail,
    ));

    let database_down = components
        .iter()
        .any(|c| c.name == "database" && c.status == HealthStatus::Down);
    let status = if database_down {
        HealthStatus::Down
    } else if components.iter().any(|c| c.status != HealthStatus::Ok) {
        HealthStatus::Degraded
    } else {
        HealthStatus::Ok
    };
    Ok(Json(EngineHealth {
        status,
        version: state.config().version.clone(),
        role: state.config().role,
        uptime_ms: u64::try_from(inner.started.elapsed().as_millis()).unwrap_or(u64::MAX),
        bind_address: state.config().listen.to_string(),
        components,
        queue,
        freshness,
        recent_errors,
        resources,
    }))
}

fn component(name: &str, status: HealthStatus, detail: &str) -> ComponentHealth {
    ComponentHealth {
        name: name.to_owned(),
        status,
        detail: detail.to_owned(),
    }
}

async fn database(store: &Store) -> (ComponentHealth, Option<QueueStats>) {
    let info = match store.check_server().await {
        Ok(info) => info,
        Err(err) => {
            tracing::warn!(error = %err, "database health check failed");
            return (
                component("database", HealthStatus::Down, "cannot reach the database"),
                None,
            );
        }
    };
    let issues = info.issues();
    let db = if issues.is_empty() {
        component(
            "database",
            HealthStatus::Ok,
            &format!("postgresql {}", info.server_version),
        )
    } else {
        let text: Vec<String> = issues.iter().map(ToString::to_string).collect();
        component("database", HealthStatus::Degraded, &text.join("; "))
    };
    let queue = match queue_stats(store).await {
        Ok(queue) => Some(queue),
        Err(err) => {
            tracing::warn!(error = %err, "queue statistics unavailable");
            None
        }
    };
    (db, queue)
}

async fn queue_stats(store: &Store) -> Result<QueueStats, ApiError> {
    let mut conn = store.acquire().await?;
    let counts = jobs::job_counts(&mut conn).await?;
    let mut stats = QueueStats {
        queued: 0,
        running: 0,
        failed: 0,
        dead_letter: 0,
        oldest_queued_ms: None,
    };
    for c in counts {
        let slot = match c.state {
            JobState::Queued => &mut stats.queued,
            JobState::Running => &mut stats.running,
            JobState::Failed => &mut stats.failed,
            JobState::Dead => &mut stats.dead_letter,
            JobState::Succeeded | JobState::Cancelled => continue,
        };
        *slot = slot.saturating_add(c.count);
    }
    // The whole queue, like `job_counts` above (the counts are not per tenant).
    if let Some(oldest) = jobs::oldest_queued(&mut conn, &JobScopeFilter::all()).await? {
        let age = OffsetDateTime::now_utc() - oldest;
        stats.oldest_queued_ms =
            Some(u64::try_from(age.whole_milliseconds().max(0)).unwrap_or(u64::MAX));
    }
    Ok(stats)
}
