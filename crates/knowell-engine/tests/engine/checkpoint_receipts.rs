//! Idempotent checkpoint saves keep their receipts in PostgreSQL: a retry
//! returns the original checkpoint after an engine restart, and concurrent
//! retries of one key store a single checkpoint.

use std::path::Path;
use std::sync::Arc;

use knowell_engine::Engine;
use knowell_index::{Priority, SyncOutcome};
use knowell_mcp::tools::{DecisionInput, InspectSymbolInput, SaveCheckpointInput};
use knowell_mcp::{KnowellTools, ResultId, SymbolRef, Target};

use crate::common::{
    TestDb, Workspace, access, alice_caller, fixture_workspace, git_available, indexer_config,
    name, require_db,
};

const PROJECT: &str = "billing-api";

async fn engine(db: &TestDb, ws: &Workspace, data: &Path) -> Engine {
    Engine::builder(db.store.clone(), indexer_config(data))
        .workspace(ws.resolved.clone())
        .access(Arc::new(access()))
        .build()
        .await
        .unwrap()
}

fn save(key: &str, evidence: &ResultId) -> SaveCheckpointInput {
    SaveCheckpointInput {
        target: Target::default(),
        goal: Some("Add a grace period to subscription cancellation".to_owned()),
        progress: "Mapped the synthetic cancellation flow".to_owned(),
        decisions: vec![DecisionInput {
            title: "Keep the old cancellation endpoint for one release".to_owned(),
            body: "Clients migrate before the endpoint is removed.".to_owned(),
            evidence: vec![evidence.clone()],
        }],
        next_steps: vec!["Change the service".to_owned()],
        idempotency_key: Some(key.to_owned()),
        ..SaveCheckpointInput::default()
    }
}

/// Rows a checkpoint save writes, for proving that a retry wrote none.
async fn counts(engine: &Engine) -> (i64, i64, i64, i64) {
    let mut conn = engine.store().acquire().await.unwrap();
    sqlx::query_as(
        "SELECT (SELECT count(*) FROM task), (SELECT sum(revision)::bigint FROM task),
                (SELECT count(*) FROM task_checkpoint),
                (SELECT count(*) FROM knowledge_record)",
    )
    .fetch_one(&mut *conn)
    .await
    .unwrap()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn checkpoint_retries_return_the_original_across_restarts_and_races() {
    if !git_available() {
        return;
    }
    let db = require_db!();
    let mut ws = fixture_workspace();
    ws.resolved
        .projects
        .retain(|project| project.name.as_str() == PROJECT);
    for project in &mut ws.resolved.projects {
        project.embedding.provider = None;
        project.embedding.model = None;
    }
    let data = tempfile::tempdir().unwrap();
    let caller = alice_caller();
    let first = engine(&db, &ws, data.path()).await;
    let (_, outcomes) = first
        .indexer()
        .index_workspace(&ws.resolved, Priority::Interactive)
        .await
        .unwrap();
    assert!(
        outcomes
            .iter()
            .all(|outcome| !matches!(outcome, SyncOutcome::Failed { .. }))
    );
    let inspected = first
        .inspect_symbol(
            &caller,
            InspectSymbolInput {
                target: Target::default(),
                symbol: SymbolRef {
                    id: None,
                    symbol: Some("SubscriptionService.cancelSubscription".to_owned()),
                    project: Some(name(PROJECT)),
                },
                include: Vec::new(),
                limit: None,
            },
        )
        .await
        .unwrap();
    let evidence = inspected.symbols[0].id.clone();

    let saved = first
        .save_checkpoint(&caller, save("restart-receipt", &evidence))
        .await
        .unwrap();
    assert!(saved.created_task && saved.created);
    assert_eq!(saved.sequence, 1);
    assert_eq!(saved.decisions.len(), 1);
    let stored = counts(&first).await;
    drop(first);

    // A fresh engine, as after a restart, answers the retry from the stored
    // receipt: same task, checkpoint and decision, and nothing new is written.
    let second = engine(&db, &ws, data.path()).await;
    let retried = second
        .save_checkpoint(&caller, save("restart-receipt", &evidence))
        .await
        .unwrap();
    assert!(!retried.created_task && !retried.created);
    assert_eq!(retried.task_id, saved.task_id);
    assert_eq!(retried.checkpoint_id, saved.checkpoint_id);
    assert_eq!(retried.sequence, 1);
    assert_eq!(
        retried.decisions.iter().map(|d| &d.id).collect::<Vec<_>>(),
        saved.decisions.iter().map(|d| &d.id).collect::<Vec<_>>()
    );
    assert_eq!(counts(&second).await, stored);

    // Concurrent first attempts of a new key store one task, one checkpoint
    // and one decision; every attempt names that checkpoint.
    let second = Arc::new(second);
    let mut handles = Vec::new();
    for _ in 0..4 {
        let engine = Arc::clone(&second);
        let caller = caller.clone();
        let input = save("racing-receipt", &evidence);
        handles.push(tokio::spawn(async move {
            engine.save_checkpoint(&caller, input).await.unwrap()
        }));
    }
    let mut outputs = Vec::new();
    for handle in handles {
        outputs.push(handle.await.unwrap());
    }
    assert_eq!(outputs.iter().filter(|o| o.created).count(), 1);
    assert!(
        outputs
            .iter()
            .all(|o| o.checkpoint_id == outputs[0].checkpoint_id && o.sequence == 1)
    );
    let (tasks, _, checkpoints, records) = counts(&second).await;
    assert_eq!(
        (tasks, checkpoints, records),
        (stored.0 + 1, stored.2 + 1, stored.3 + 1)
    );
}
