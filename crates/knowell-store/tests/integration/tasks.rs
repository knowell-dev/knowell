use std::collections::BTreeSet;

use knowell_store::knowledge::{self, NewRecord, RecordScope};
use knowell_store::tasks::*;
use knowell_store::{
    CheckpointReceiptId, KnowledgeKind, KnowledgeRecordId, KnowledgeState, StoreError, TaskId,
    TaskStatus, WorkspaceId, hierarchy,
};
use serde_json::json;
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use crate::common::{Fixture, fixture, require_db};

const T0: OffsetDateTime = datetime!(2026-10-02 12:00 UTC);

fn t(minutes: i64) -> OffsetDateTime {
    T0 + time::Duration::minutes(minutes)
}

fn task(fx: &Fixture, title: &str, owner: Option<&str>) -> NewTask {
    NewTask {
        id: TaskId(Uuid::now_v7()),
        organization: fx.org.id,
        workspace: Some(fx.workspace.id),
        owner: owner.map(str::to_owned),
        title: title.into(),
        goal: "Ship it".into(),
        status: TaskStatus::Open,
        details: TaskDetails::default(),
        created_at: T0,
        updated_at: T0,
    }
}

fn manifest() -> serde_json::Value {
    json!([{"project": "shop", "view": "branch:main", "commit": "abc1234", "local_generation": null}])
}

#[tokio::test]
async fn tasks_crud_with_optimistic_concurrency() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let new = task(&fx, "Migrate payments", Some("u-1"));
    let created = create_task(&mut c, &new).await.unwrap();
    assert_eq!(created.id, new.id);
    assert_eq!(created.revision, 1);
    assert_eq!(created.details, TaskDetails::default());
    assert_eq!(
        get_task(&mut c, new.id).await.unwrap(),
        Some(created.clone())
    );
    assert!(matches!(
        create_task(&mut c, &new).await,
        Err(StoreError::AlreadyExists { .. })
    ));

    let decision = KnowledgeRecordId(Uuid::now_v7());
    let update = TaskUpdate {
        id: new.id,
        expected_revision: created.revision,
        title: "Migrate payments to v2".into(),
        goal: "All callers on v2".into(),
        status: TaskStatus::InProgress,
        details: TaskDetails {
            notes: json!([{"at": 1, "author": "system", "text": "started"}]),
            decisions: vec![decision],
            open_questions: json!([{"id": 1, "text": "Which region first?"}]),
            related_symbols: vec!["shop::pay".into()],
            related_files: json!([{"project": "shop", "path": "src/pay.rs"}]),
            view_manifest: manifest(),
        },
        updated_at: t(1),
    };
    let updated = update_task(&mut c, &update).await.unwrap();
    assert_eq!(updated.revision, 2);
    assert_eq!(updated.status, TaskStatus::InProgress);
    assert_eq!(updated.details, update.details);
    assert_eq!(updated.owner.as_deref(), Some("u-1"));
    // The same update again is based on a stale revision.
    let err = update_task(&mut c, &update).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }), "{err}");
    let unknown = TaskUpdate {
        id: TaskId(Uuid::now_v7()),
        ..update.clone()
    };
    assert!(matches!(
        update_task(&mut c, &unknown).await,
        Err(StoreError::NotFound { .. })
    ));
    let not_a_list = TaskUpdate {
        expected_revision: 2,
        details: TaskDetails {
            notes: json!("text"),
            ..TaskDetails::default()
        },
        ..update.clone()
    };
    assert!(matches!(
        update_task(&mut c, &not_a_list).await,
        Err(StoreError::InvalidInput(_))
    ));

    // Listing: newest update first, filters, pages.
    let mut second = task(&fx, "Write docs", Some("u-2"));
    second.updated_at = t(5);
    create_task(&mut c, &second).await.unwrap();
    let mut third = task(&fx, "Old chore", None);
    third.status = TaskStatus::Done;
    third.workspace = None;
    create_task(&mut c, &third).await.unwrap();
    let titles = |tasks: Vec<StoredTask>| tasks.into_iter().map(|t| t.title).collect::<Vec<_>>();
    assert_eq!(
        titles(
            list_tasks(&mut c, &TaskFilter::new(fx.org.id, 10))
                .await
                .unwrap()
        ),
        ["Write docs", "Migrate payments to v2", "Old chore"]
    );
    let mine = TaskFilter {
        owner: Some("u-1".into()),
        ..TaskFilter::new(fx.org.id, 10)
    };
    assert_eq!(
        titles(list_tasks(&mut c, &mine).await.unwrap()),
        ["Migrate payments to v2"]
    );
    let open = TaskFilter {
        statuses: vec![TaskStatus::Open, TaskStatus::InProgress],
        workspace: Some(fx.workspace.id),
        ..TaskFilter::new(fx.org.id, 10)
    };
    assert_eq!(list_tasks(&mut c, &open).await.unwrap().len(), 2);
    let first_page = list_tasks(&mut c, &TaskFilter::new(fx.org.id, 2))
        .await
        .unwrap();
    let rest = list_tasks(
        &mut c,
        &TaskFilter {
            before: first_page.last().map(TaskCursor::after),
            ..TaskFilter::new(fx.org.id, 2)
        },
    )
    .await
    .unwrap();
    assert_eq!(titles(rest), ["Old chore"]);
    assert!(matches!(
        list_tasks(&mut c, &TaskFilter::new(fx.org.id, 0)).await,
        Err(StoreError::InvalidInput(_))
    ));

    // Tenancy: a workspace of another organization is not found.
    let other = fixture(&mut c, "other").await;
    let mut foreign = task(&fx, "Foreign", None);
    foreign.workspace = Some(other.workspace.id);
    assert!(matches!(
        create_task(&mut c, &foreign).await,
        Err(StoreError::NotFound { .. })
    ));
    let mut nowhere = task(&fx, "Nowhere", None);
    nowhere.workspace = Some(WorkspaceId(Uuid::now_v7()));
    assert!(matches!(
        create_task(&mut c, &nowhere).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(
        list_tasks(&mut c, &TaskFilter::new(other.org.id, 10))
            .await
            .unwrap()
            .is_empty()
    );

    assert!(delete_task(&mut c, new.id).await.unwrap());
    assert!(!delete_task(&mut c, new.id).await.unwrap());
    // Deleting the workspace deletes its tasks.
    hierarchy::delete_workspace(&mut c, fx.workspace.id)
        .await
        .unwrap();
    assert_eq!(get_task(&mut c, second.id).await.unwrap(), None);
    assert!(get_task(&mut c, third.id).await.unwrap().is_some());
}

#[tokio::test]
async fn checkpoints_are_numbered_append_only_and_cascade() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let new = task(&fx, "Migrate", Some("u-1"));
    create_task(&mut c, &new).await.unwrap();
    assert_eq!(latest_checkpoint(&mut c, new.id).await.unwrap(), None);

    let first = append_checkpoint(
        &mut c,
        &NewCheckpoint {
            task: new.id,
            at: t(1),
            summary: "Mapped the callers".into(),
            decisions: vec![KnowledgeRecordId(Uuid::now_v7())],
            next_steps: vec!["Update the client".into()],
            manifest: manifest(),
        },
    )
    .await
    .unwrap();
    assert_eq!(first.seq, 1);
    assert_eq!(first.manifest, manifest());

    // Concurrent appends get distinct, gap-free numbers.
    let mut handles = Vec::new();
    for n in 0..8 {
        let store = db.store.clone();
        let id = new.id;
        handles.push(tokio::spawn(async move {
            let mut c = store.acquire().await.unwrap();
            append_checkpoint(
                &mut c,
                &NewCheckpoint {
                    task: id,
                    at: t(2 + n),
                    summary: format!("Step {n}"),
                    decisions: Vec::new(),
                    next_steps: Vec::new(),
                    manifest: json!([]),
                },
            )
            .await
            .unwrap()
            .seq
        }));
    }
    let mut seqs = BTreeSet::new();
    for h in handles {
        seqs.insert(h.await.unwrap());
    }
    assert_eq!(seqs, (2..=9).collect::<BTreeSet<u64>>());
    let all = list_checkpoints(&mut c, new.id).await.unwrap();
    assert_eq!(
        all.iter().map(|c| c.seq).collect::<Vec<_>>(),
        (1..=9).collect::<Vec<u64>>()
    );
    assert_eq!(all[0], first);
    assert_eq!(
        latest_checkpoint(&mut c, new.id)
            .await
            .unwrap()
            .unwrap()
            .seq,
        9
    );

    // Invalid input and unknown tasks.
    let bad = NewCheckpoint {
        task: new.id,
        at: t(20),
        summary: String::new(),
        decisions: Vec::new(),
        next_steps: Vec::new(),
        manifest: json!([]),
    };
    assert!(matches!(
        append_checkpoint(&mut c, &bad).await,
        Err(StoreError::InvalidInput(_))
    ));
    let bad = NewCheckpoint {
        summary: "x".into(),
        manifest: json!({"pins": []}),
        ..bad
    };
    assert!(matches!(
        append_checkpoint(&mut c, &bad).await,
        Err(StoreError::InvalidInput(_))
    ));
    let unknown = NewCheckpoint {
        task: TaskId(Uuid::now_v7()),
        manifest: json!([]),
        ..bad
    };
    assert!(matches!(
        append_checkpoint(&mut c, &unknown).await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(
        sqlx::query("UPDATE task_checkpoint SET summary = 'rewritten'")
            .execute(&mut *c)
            .await
            .is_err()
    );

    // A task-scoped record goes away with its task, like the checkpoints.
    let record = knowledge::insert_record(
        &mut c,
        &NewRecord {
            id: knowledge::new_record_id(),
            organization: fx.org.id,
            scope: RecordScope::Task(new.id),
            kind: KnowledgeKind::ModelSuggestion,
            subject: "migration.notes".into(),
            title: "Callers".into(),
            body: "Three callers.".into(),
            state: KnowledgeState::Proposed,
            version: 1,
            author: json!({"agent": {"session": "s-1", "client": "test-agent"}}),
            pinned: false,
            tags: Vec::new(),
            related_symbols: Vec::new(),
            superseded_by: None,
            evidence: Vec::new(),
            history: Vec::new(),
            created_at: T0,
            updated_at: T0,
        },
    )
    .await
    .unwrap();
    assert!(delete_task(&mut c, new.id).await.unwrap());
    assert!(list_checkpoints(&mut c, new.id).await.unwrap().is_empty());
    assert_eq!(
        knowledge::get_record(&mut c, record.id).await.unwrap(),
        None
    );
}

/// A task decision as the engine stores it with a checkpoint.
fn decision(fx: &Fixture, task: TaskId, title: &str) -> NewRecord {
    NewRecord {
        id: knowledge::new_record_id(),
        organization: fx.org.id,
        scope: RecordScope::Task(task),
        kind: KnowledgeKind::ModelSuggestion,
        subject: "migration.decision".into(),
        title: title.into(),
        body: "Synthetic decision body.".into(),
        state: KnowledgeState::Proposed,
        version: 1,
        author: json!({"agent": {"session": "s-1", "client": "test-agent"}}),
        pinned: false,
        tags: vec!["decision".into()],
        related_symbols: Vec::new(),
        superseded_by: None,
        evidence: Vec::new(),
        history: Vec::new(),
        created_at: T0,
        updated_at: T0,
    }
}

/// The task update and checkpoint of one save recording `decision`.
fn checkpoint_parts(
    created: &StoredTask,
    decision: KnowledgeRecordId,
) -> (TaskUpdate, NewCheckpoint) {
    let update = TaskUpdate {
        id: created.id,
        expected_revision: created.revision,
        title: created.title.clone(),
        goal: created.goal.clone(),
        status: TaskStatus::InProgress,
        details: TaskDetails {
            decisions: vec![decision],
            view_manifest: manifest(),
            ..TaskDetails::default()
        },
        updated_at: t(1),
    };
    let checkpoint = NewCheckpoint {
        task: created.id,
        at: t(1),
        summary: "Mapped the callers".into(),
        decisions: vec![decision],
        next_steps: vec!["Update the client".into()],
        manifest: manifest(),
    };
    (update, checkpoint)
}

#[tokio::test]
async fn checkpoint_receipts_replay_saves_and_stay_in_their_organization() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let new = task(&fx, "Migrate", Some("u-1"));
    let created = create_task(&mut c, &new).await.unwrap();
    let record = decision(&fx, new.id, "Keep v1 for one release");
    let (update, checkpoint) = checkpoint_parts(&created, record.id);
    let receipt = CheckpointReceiptId(Uuid::now_v7());

    let CheckpointWrite::Saved(saved) = save_checkpoint(
        &mut c,
        fx.org.id,
        Some(receipt),
        std::slice::from_ref(&record),
        &update,
        &checkpoint,
    )
    .await
    .unwrap() else {
        panic!("the first save stores its checkpoint");
    };
    assert_eq!(saved.seq, 1);
    assert_eq!(saved.task.revision, 2);
    assert_eq!(saved.task.details.decisions, vec![record.id]);
    assert_eq!(
        saved.decisions.iter().map(|r| r.id).collect::<Vec<_>>(),
        vec![record.id]
    );
    let expected = CheckpointReceipt {
        task: new.id,
        seq: 1,
    };
    assert_eq!(
        find_checkpoint_receipt(&mut c, fx.org.id, receipt)
            .await
            .unwrap(),
        Some(expected)
    );

    // A retry (same receipt, now stale revision, freshly generated decision)
    // stores nothing and names the original checkpoint.
    let retried = decision(&fx, new.id, "Keep v1 for one release");
    let replay = save_checkpoint(
        &mut c,
        fx.org.id,
        Some(receipt),
        std::slice::from_ref(&retried),
        &update,
        &checkpoint,
    )
    .await
    .unwrap();
    assert_eq!(replay, CheckpointWrite::Replayed(expected));
    assert_eq!(list_checkpoints(&mut c, new.id).await.unwrap().len(), 1);
    assert_eq!(get_task(&mut c, new.id).await.unwrap().unwrap().revision, 2);
    assert_eq!(
        knowledge::get_record(&mut c, retried.id).await.unwrap(),
        None
    );

    // Another organization neither sees the receipt nor can point one at
    // this checkpoint; a receipt cannot name a checkpoint that does not exist.
    let other = fixture(&mut c, "other").await;
    assert_eq!(
        find_checkpoint_receipt(&mut c, other.org.id, receipt)
            .await
            .unwrap(),
        None
    );
    assert!(matches!(
        insert_checkpoint_receipt(
            &mut c,
            other.org.id,
            CheckpointReceiptId(Uuid::now_v7()),
            expected
        )
        .await,
        Err(StoreError::NotFound { .. })
    ));
    assert!(matches!(
        insert_checkpoint_receipt(
            &mut c,
            fx.org.id,
            CheckpointReceiptId(Uuid::now_v7()),
            CheckpointReceipt {
                task: new.id,
                seq: 2
            }
        )
        .await,
        Err(StoreError::NotFound { .. })
    ));

    // Receipts are never rewritten and go away with their task.
    assert!(
        sqlx::query("UPDATE checkpoint_receipt SET created_at = now()")
            .execute(&mut *c)
            .await
            .is_err()
    );
    assert!(delete_task(&mut c, new.id).await.unwrap());
    assert_eq!(
        find_checkpoint_receipt(&mut c, fx.org.id, receipt)
            .await
            .unwrap(),
        None
    );
}

/// Asserts that nothing of a save of `record` into `task` was stored.
async fn assert_nothing_saved(c: &mut sqlx::PgConnection, task: TaskId, record: KnowledgeRecordId) {
    assert_eq!(knowledge::get_record(&mut *c, record).await.unwrap(), None);
    assert_eq!(get_task(&mut *c, task).await.unwrap().unwrap().revision, 1);
    assert!(list_checkpoints(&mut *c, task).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_failed_checkpoint_save_stores_nothing() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let new = task(&fx, "Migrate", None);
    let created = create_task(&mut c, &new).await.unwrap();
    let record = decision(&fx, new.id, "Batch the backfill");
    let (update, checkpoint) = checkpoint_parts(&created, record.id);
    let receipt = CheckpointReceiptId(Uuid::now_v7());

    // The checkpoint is rejected after the decision and the task update were
    // written in the transaction: both are rolled back and no receipt is kept.
    let invalid = NewCheckpoint {
        summary: String::new(),
        ..checkpoint.clone()
    };
    assert!(matches!(
        save_checkpoint(
            &mut c,
            fx.org.id,
            Some(receipt),
            std::slice::from_ref(&record),
            &update,
            &invalid
        )
        .await,
        Err(StoreError::InvalidInput(_))
    ));
    assert_nothing_saved(&mut c, new.id, record.id).await;
    assert_eq!(
        find_checkpoint_receipt(&mut c, fx.org.id, receipt)
            .await
            .unwrap(),
        None
    );

    // A stale task revision is a conflict, again without partial writes.
    let stale = TaskUpdate {
        expected_revision: 7,
        ..update.clone()
    };
    assert!(matches!(
        save_checkpoint(
            &mut c,
            fx.org.id,
            Some(receipt),
            std::slice::from_ref(&record),
            &stale,
            &checkpoint
        )
        .await,
        Err(StoreError::Conflict { .. })
    ));
    assert_nothing_saved(&mut c, new.id, record.id).await;

    // The corrected save then succeeds under the same receipt.
    assert!(matches!(
        save_checkpoint(
            &mut c,
            fx.org.id,
            Some(receipt),
            std::slice::from_ref(&record),
            &update,
            &checkpoint
        )
        .await
        .unwrap(),
        CheckpointWrite::Saved(_)
    ));
    assert!(
        knowledge::get_record(&mut c, record.id)
            .await
            .unwrap()
            .is_some()
    );
}

#[tokio::test]
async fn concurrent_saves_with_one_receipt_store_one_checkpoint() {
    const ATTEMPTS: usize = 4;
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let new = task(&fx, "Migrate", Some("u-1"));
    let created = create_task(&mut c, &new).await.unwrap();
    let receipt = CheckpointReceiptId(Uuid::now_v7());
    let barrier = std::sync::Arc::new(tokio::sync::Barrier::new(ATTEMPTS));
    let mut handles = Vec::new();
    for _ in 0..ATTEMPTS {
        let store = db.store.clone();
        let barrier = std::sync::Arc::clone(&barrier);
        // Every attempt generates its own decision id, as a retry would.
        let record = decision(&fx, new.id, "Batch the backfill");
        let (update, checkpoint) = checkpoint_parts(&created, record.id);
        let organization = fx.org.id;
        handles.push(tokio::spawn(async move {
            let mut c = store.acquire().await.unwrap();
            barrier.wait().await;
            let written = save_checkpoint(
                &mut c,
                organization,
                Some(receipt),
                std::slice::from_ref(&record),
                &update,
                &checkpoint,
            )
            .await
            .unwrap();
            (written, record.id)
        }));
    }
    let mut winners = Vec::new();
    let mut losers = Vec::new();
    for handle in handles {
        match handle.await.unwrap() {
            (CheckpointWrite::Saved(saved), id) => winners.push((saved.seq, id)),
            (CheckpointWrite::Replayed(found), id) => losers.push((found, id)),
        }
    }
    assert_eq!(winners.len(), 1, "exactly one attempt stores");
    let (seq, kept) = winners[0];
    assert_eq!(seq, 1);
    for (found, discarded) in &losers {
        assert_eq!(
            *found,
            CheckpointReceipt {
                task: new.id,
                seq: 1
            }
        );
        assert_eq!(
            knowledge::get_record(&mut c, *discarded).await.unwrap(),
            None
        );
    }
    assert!(knowledge::get_record(&mut c, kept).await.unwrap().is_some());
    assert_eq!(list_checkpoints(&mut c, new.id).await.unwrap().len(), 1);
    assert_eq!(get_task(&mut c, new.id).await.unwrap().unwrap().revision, 2);
}
