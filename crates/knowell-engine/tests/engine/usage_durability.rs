//! Tool usage survives an engine restart: buffered calls reach the store,
//! and the usage report and integration status read them back.

use std::sync::Arc;
use std::time::Duration;

use knowell_auth::{Grant, GrantSet, Principal, RequestId, ResourceScope, Role, visible_projects};
use knowell_engine::{Engine, EngineSettings};
use knowell_mcp::KnowellTools;
use knowell_mcp::tools::OpenWorkspaceInput;
use knowell_server::{EngineContext, EngineRequest, MemoryAuditSink};
use knowell_store::{hierarchy, usage};
use serde_json::Value;
use time::OffsetDateTime;

use crate::common::{TestDb, access, alice, alice_caller, indexer_config, name, require_db};

async fn engine(db: &TestDb, data: &std::path::Path) -> Engine {
    Engine::builder(db.store.clone(), indexer_config(data))
        .settings(EngineSettings {
            // Only explicit flushes, so the test controls what is stored.
            usage_flush_interval: Duration::from_secs(3600),
            ..EngineSettings::default()
        })
        .access(Arc::new(access()))
        .build()
        .await
        .unwrap()
}

fn viewer() -> EngineContext {
    let principal = Principal::User(alice());
    let mut grants = GrantSet::new();
    grants.add(Grant::new(principal.clone(), Role::Viewer, ResourceScope::Organization).unwrap());
    let visible = visible_projects(&principal, &grants);
    EngineContext {
        principal,
        scopes: None,
        grants: Arc::new(grants),
        visible,
        request_id: RequestId::new("synthetic-usage-durability").unwrap(),
        audit: Arc::new(MemoryAuditSink::new()),
    }
}

async fn report(engine: &Engine, days: u32) -> Value {
    knowell_server::Engine::call(engine, &viewer(), EngineRequest::Usage { days })
        .await
        .unwrap()
}

fn tool_calls(report: &Value) -> Vec<(String, u64)> {
    report["tools"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| {
            (
                t["tool"].as_str().unwrap().to_owned(),
                t["calls"].as_u64().unwrap(),
            )
        })
        .collect()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn tool_usage_survives_restarts_and_respects_the_period() {
    let db = require_db!();
    let data = tempfile::tempdir().unwrap();
    let first = engine(&db, data.path()).await;
    for _ in 0..3 {
        // Failed calls count too; only the call itself matters here.
        let _ = first
            .open_workspace(&alice_caller(), OpenWorkspaceInput::default())
            .await;
    }
    // Buffered calls are reported before they are stored.
    let buffered = report(&first, 1).await;
    assert_eq!(tool_calls(&buffered), [("open_workspace".to_owned(), 3)]);
    first.flush_usage().await.unwrap();
    // A second flush has nothing left to add.
    first.flush_usage().await.unwrap();
    assert_eq!(report(&first, 1).await, buffered);
    drop(first);

    // A fresh engine, as after a restart, reads the stored usage.
    let second = engine(&db, data.path()).await;
    let restarted = report(&second, 1).await;
    assert_eq!(restarted, buffered);
    assert_eq!(restarted["agents"][0]["agent"], "claude-code");
    assert_eq!(restarted["agents"][0]["calls"], 3);
    assert_eq!(restarted["daily"][0]["calls"], 3);
    let integrations =
        knowell_server::Engine::call(&second, &viewer(), EngineRequest::Integrations)
            .await
            .unwrap();
    assert!(
        integrations["mcp"]["lastCallAt"].is_string(),
        "{integrations}"
    );

    // Older hours count only in periods that reach them, for tools as well.
    let mut conn = db.store.acquire().await.unwrap();
    let org = hierarchy::find_organization(&mut conn, &name("acme"))
        .await
        .unwrap()
        .unwrap();
    let old = OffsetDateTime::now_utc() - time::Duration::days(10);
    let mut latency_buckets = vec![0; usage::LATENCY_BUCKETS];
    latency_buckets[usage::latency_bucket(40)] = 2;
    usage::record_tool_usage(
        &mut conn,
        org.id,
        &[usage::ToolUsage {
            hour: usage::hour_of(old).unwrap(),
            tool: "search".to_owned(),
            agent: "codex".to_owned(),
            calls: 2,
            errors: 0,
            tokens_returned: 50,
            latency_ms_sum: 80,
            latency_buckets,
            last_call_at: old,
        }],
    )
    .await
    .unwrap();
    drop(conn);
    assert_eq!(
        tool_calls(&report(&second, 7).await),
        [("open_workspace".to_owned(), 3)]
    );
    assert_eq!(
        tool_calls(&report(&second, 30).await),
        [("open_workspace".to_owned(), 3), ("search".to_owned(), 2)]
    );
}
