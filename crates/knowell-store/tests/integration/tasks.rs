use std::collections::BTreeSet;

use knowell_store::knowledge::{self, NewRecord, RecordScope};
use knowell_store::tasks::*;
use knowell_store::{
    KnowledgeKind, KnowledgeRecordId, KnowledgeState, StoreError, TaskId, TaskStatus, WorkspaceId,
    hierarchy,
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
