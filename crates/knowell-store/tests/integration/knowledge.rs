use std::collections::BTreeSet;

use knowell_core::{ContentHash, LineRange};
use knowell_store::knowledge::*;
use knowell_store::tasks::{self, NewTask, TaskDetails};
use knowell_store::{
    KnowledgeAction, KnowledgeKind, KnowledgeRecordId, KnowledgeState, StoreError, TaskId,
    TaskStatus, hierarchy,
};
use serde_json::json;
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use crate::common::{Fixture, add_project, fixture, path, require_db};

const T0: OffsetDateTime = datetime!(2026-10-02 12:00 UTC);

fn t(minutes: i64) -> OffsetDateTime {
    T0 + time::Duration::minutes(minutes)
}

fn hash(text: &str) -> ContentHash {
    ContentHash::of(text.as_bytes())
}

fn evidence(fx: &Fixture, file: &str, content: &str) -> RecordEvidence {
    RecordEvidence {
        project: fx.project.id,
        view: "branch:main".into(),
        commit: "abc1234".into(),
        path: path(file),
        lines: LineRange::new(3, 9).unwrap(),
        content_hash: hash(content),
    }
}

fn entry(
    at: OffsetDateTime,
    action: KnowledgeAction,
    from: Option<KnowledgeState>,
    to: KnowledgeState,
    version: u32,
) -> HistoryEntry {
    HistoryEntry {
        at,
        actor: json!({"human": "u-1"}),
        action,
        from,
        to,
        version,
        reason: format!("{} by test", action.as_str()),
    }
}

fn record(fx: &Fixture, scope: RecordScope, subject: &str, title: &str, body: &str) -> NewRecord {
    NewRecord {
        id: new_record_id(),
        organization: fx.org.id,
        scope,
        kind: KnowledgeKind::Human,
        subject: subject.into(),
        title: title.into(),
        body: body.into(),
        state: KnowledgeState::Proposed,
        version: 1,
        author: json!({"human": "u-1"}),
        pinned: false,
        tags: Vec::new(),
        related_symbols: Vec::new(),
        superseded_by: None,
        evidence: Vec::new(),
        history: vec![entry(
            T0,
            KnowledgeAction::Propose,
            None,
            KnowledgeState::Proposed,
            1,
        )],
        created_at: T0,
        updated_at: T0,
    }
}

/// An update that changes nothing but the state, based on `stored`.
fn state_change(stored: &StoredRecord, to: KnowledgeState, at: OffsetDateTime) -> RecordUpdate {
    RecordUpdate {
        id: stored.id,
        expected_version: stored.version,
        expected_revision: stored.revision,
        state: to,
        pinned: stored.pinned,
        related_symbols: stored.related_symbols.clone(),
        superseded_by: stored.superseded_by,
        content: None,
        history: vec![entry(
            at,
            KnowledgeAction::Accept,
            Some(stored.state),
            to,
            stored.version,
        )],
        updated_at: at,
    }
}

#[tokio::test]
async fn records_round_trip_with_versions_evidence_and_history() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let mut new = record(
        &fx,
        RecordScope::Project(fx.project.id),
        "payments.idempotency",
        "Retries reuse the idempotency key",
        "Every POST to /payments carries `Idempotency-Key`.",
    );
    new.kind = KnowledgeKind::Observed;
    new.state = KnowledgeState::Accepted;
    new.tags = vec!["payments".into(), "rule".into()];
    new.related_symbols = vec!["shop::payments::retry".into()];
    new.evidence = vec![
        evidence(&fx, "src/payments.rs", "v1"),
        evidence(&fx, "src/retry.rs", "r1"),
    ];
    new.history.push(entry(
        T0,
        KnowledgeAction::Accept,
        Some(KnowledgeState::Proposed),
        KnowledgeState::Accepted,
        1,
    ));
    let stored = insert_record(&mut c, &new).await.unwrap();
    assert_eq!(stored.id, new.id);
    assert_eq!(stored.scope, RecordScope::Project(fx.project.id));
    assert_eq!(stored.workspace, Some(fx.workspace.id));
    assert_eq!(stored.version, 1);
    assert_eq!(stored.revision, 1);
    assert_eq!(stored.evidence, new.evidence);
    assert_eq!(stored.tags, new.tags);
    assert_eq!(stored.author, json!({"human": "u-1"}));
    assert_eq!(stored.created_at, T0);
    assert_eq!(get_record(&mut c, new.id).await.unwrap(), Some(stored));

    let versions = record_versions(&mut c, new.id).await.unwrap();
    assert_eq!(versions.len(), 1);
    assert_eq!(versions[0].version, 1);
    assert_eq!(versions[0].evidence, new.evidence);
    assert_eq!(versions[0].replaced_at, None);
    assert_eq!(record_history(&mut c, new.id).await.unwrap(), new.history);

    // Same id again.
    assert!(matches!(
        insert_record(&mut c, &new).await,
        Err(StoreError::AlreadyExists { .. })
    ));
    assert_eq!(
        get_record(&mut c, KnowledgeRecordId(Uuid::now_v7()))
            .await
            .unwrap(),
        None
    );
    assert!(delete_record(&mut c, new.id).await.unwrap());
    assert!(!delete_record(&mut c, new.id).await.unwrap());
    assert!(record_versions(&mut c, new.id).await.unwrap().is_empty());
    assert!(record_history(&mut c, new.id).await.unwrap().is_empty());
}

#[tokio::test]
async fn updates_use_optimistic_concurrency_and_append_versions() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let mut new = record(
        &fx,
        RecordScope::Workspace(fx.workspace.id),
        "api.errors",
        "Errors are problem+json",
        "All HTTP errors use RFC 7807.",
    );
    new.evidence = vec![evidence(&fx, "src/errors.rs", "e1")];
    let v1 = insert_record(&mut c, &new).await.unwrap();

    // A state change keeps the version and bumps the revision.
    let accepted = update_record(&mut c, &state_change(&v1, KnowledgeState::Accepted, t(1)))
        .await
        .unwrap();
    assert_eq!(accepted.state, KnowledgeState::Accepted);
    assert_eq!((accepted.version, accepted.revision), (1, 2));
    assert_eq!(accepted.evidence, v1.evidence);
    assert_eq!(accepted.updated_at, t(1));

    // A writer that read the old revision loses.
    let stale = state_change(&v1, KnowledgeState::Rejected, t(2));
    let err = update_record(&mut c, &stale).await.unwrap_err();
    assert!(matches!(err, StoreError::Conflict { .. }), "{err}");
    assert!(err.to_string().contains("revision 2"), "{err}");
    assert_eq!(
        get_record(&mut c, v1.id).await.unwrap().unwrap().state,
        KnowledgeState::Accepted
    );

    // An edit stores version 2 with new evidence; version 1 stays intact.
    let edit = RecordUpdate {
        content: Some(RecordContent {
            title: "Errors are RFC 7807 problem+json".into(),
            body: "All HTTP errors use RFC 7807 with a `code`.".into(),
            tags: vec!["api".into()],
            evidence: vec![
                evidence(&fx, "src/errors.rs", "e2"),
                evidence(&fx, "src/problem.rs", "p1"),
            ],
        }),
        history: vec![entry(
            t(3),
            KnowledgeAction::Edit,
            Some(KnowledgeState::Accepted),
            KnowledgeState::Proposed,
            2,
        )],
        ..state_change(&accepted, KnowledgeState::Proposed, t(3))
    };
    let v2 = update_record(&mut c, &edit).await.unwrap();
    assert_eq!((v2.version, v2.revision), (2, 3));
    assert_eq!(v2.title, "Errors are RFC 7807 problem+json");
    assert_eq!(v2.tags, vec!["api".to_owned()]);
    assert_eq!(v2.evidence.len(), 2);
    let versions = record_versions(&mut c, v1.id).await.unwrap();
    assert_eq!(versions.len(), 2);
    assert_eq!(versions[0].title, "Errors are problem+json");
    assert_eq!(versions[0].evidence, v1.evidence);
    assert_eq!(versions[0].replaced_at, Some(t(3)));
    assert_eq!(versions[1].created_at, t(3));
    assert_eq!(versions[1].replaced_at, None);
    let history = record_history(&mut c, v1.id).await.unwrap();
    assert_eq!(
        history.iter().map(|h| h.action).collect::<Vec<_>>(),
        [
            KnowledgeAction::Propose,
            KnowledgeAction::Accept,
            KnowledgeAction::Edit
        ]
    );

    // Expecting the old content version is a conflict too.
    let mut old_version = state_change(&v2, KnowledgeState::Accepted, t(4));
    old_version.expected_version = 1;
    assert!(matches!(
        update_record(&mut c, &old_version).await,
        Err(StoreError::Conflict { .. })
    ));
    // Unknown records and self-supersession.
    let mut unknown = state_change(&v2, KnowledgeState::Accepted, t(4));
    unknown.id = KnowledgeRecordId(Uuid::now_v7());
    assert!(matches!(
        update_record(&mut c, &unknown).await,
        Err(StoreError::NotFound { .. })
    ));
    let mut itself = state_change(&v2, KnowledgeState::Superseded, t(4));
    itself.superseded_by = Some(v2.id);
    assert!(matches!(
        update_record(&mut c, &itself).await,
        Err(StoreError::InvalidInput(_))
    ));

    // Stored versions, evidence and history are immutable.
    for sql in [
        "UPDATE knowledge_record_version SET body = 'x'",
        "UPDATE knowledge_evidence SET start_line = 1",
        "UPDATE knowledge_history SET reason = 'x'",
    ] {
        assert!(sqlx::query(sql).execute(&mut *c).await.is_err(), "{sql}");
    }
}

#[tokio::test]
async fn concurrent_reviewers_cannot_both_win() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let stored = insert_record(
        &mut c,
        &record(
            &fx,
            RecordScope::Organization,
            "release.process",
            "Tag",
            "Body",
        ),
    )
    .await
    .unwrap();
    let mut handles = Vec::new();
    for (n, to) in [KnowledgeState::Accepted, KnowledgeState::Rejected]
        .into_iter()
        .enumerate()
    {
        let store = db.store.clone();
        let update = state_change(&stored, to, t(i64::try_from(n).unwrap() + 1));
        handles.push(tokio::spawn(async move {
            let mut c = store.acquire().await.unwrap();
            update_record(&mut c, &update).await
        }));
    }
    let mut won = 0;
    for h in handles {
        match h.await.unwrap() {
            Ok(r) => {
                won += 1;
                assert_eq!(r.revision, 2);
            }
            Err(StoreError::Conflict { .. }) => {}
            Err(other) => panic!("unexpected error: {other}"),
        }
    }
    assert_eq!(won, 1);
    assert_eq!(record_history(&mut c, stored.id).await.unwrap().len(), 2);
}

#[tokio::test]
async fn listings_filter_by_scope_state_kind_subject_tag_and_page() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let (web, _) = add_project(&mut c, &fx, "web").await;
    let task_id = TaskId(Uuid::now_v7());
    tasks::create_task(
        &mut c,
        &NewTask {
            id: task_id,
            organization: fx.org.id,
            workspace: Some(fx.workspace.id),
            owner: Some("u-1".into()),
            title: "Migrate".into(),
            goal: "Move to v2".into(),
            status: TaskStatus::Open,
            details: TaskDetails::default(),
            created_at: T0,
            updated_at: T0,
        },
    )
    .await
    .unwrap();
    let scopes = [
        RecordScope::Organization,
        RecordScope::Workspace(fx.workspace.id),
        RecordScope::Project(fx.project.id),
        RecordScope::Project(web.id),
        RecordScope::Task(task_id),
        RecordScope::User("u-1".into()),
    ];
    let mut all = Vec::new();
    for (n, scope) in scopes.iter().enumerate() {
        let minute = i64::try_from(n).unwrap();
        let mut r = record(
            &fx,
            scope.clone(),
            "topic.a",
            &format!("Record {n}"),
            "Body",
        );
        r.updated_at = t(minute);
        r.state = if n % 2 == 0 {
            KnowledgeState::Accepted
        } else {
            KnowledgeState::Proposed
        };
        if n == 2 {
            r.subject = "topic.b".into();
            r.kind = KnowledgeKind::ModelSuggestion;
            r.pinned = true;
            r.tags = vec!["rule".into()];
        }
        all.push(insert_record(&mut c, &r).await.unwrap());
    }

    let list = |filter: RecordFilter| {
        let store = db.store.clone();
        async move {
            let mut c = store.acquire().await.unwrap();
            list_records(&mut c, &filter).await.unwrap()
        }
    };
    let titles = |records: &[StoredRecord]| -> Vec<String> {
        records.iter().map(|r| r.title.clone()).collect()
    };
    // Newest update first.
    let everything = list(RecordFilter::new(fx.org.id, 100)).await;
    assert_eq!(
        titles(&everything),
        [
            "Record 5", "Record 4", "Record 3", "Record 2", "Record 1", "Record 0"
        ]
    );
    // What a caller in project `shop` sees: org + workspace + project.
    let visible = list(RecordFilter {
        scopes: vec![
            RecordScope::Organization,
            RecordScope::Workspace(fx.workspace.id),
            RecordScope::Project(fx.project.id),
        ],
        ..RecordFilter::new(fx.org.id, 100)
    })
    .await;
    assert_eq!(titles(&visible), ["Record 2", "Record 1", "Record 0"]);
    let accepted = list(RecordFilter {
        states: vec![KnowledgeState::Accepted],
        ..RecordFilter::new(fx.org.id, 100)
    })
    .await;
    assert_eq!(titles(&accepted), ["Record 4", "Record 2", "Record 0"]);
    for filter in [
        RecordFilter {
            subject: Some("topic.b".into()),
            ..RecordFilter::new(fx.org.id, 100)
        },
        RecordFilter {
            kinds: vec![KnowledgeKind::ModelSuggestion],
            ..RecordFilter::new(fx.org.id, 100)
        },
        RecordFilter {
            pinned: Some(true),
            ..RecordFilter::new(fx.org.id, 100)
        },
        RecordFilter {
            tag: Some("rule".into()),
            ..RecordFilter::new(fx.org.id, 100)
        },
    ] {
        assert_eq!(titles(&list(filter).await), ["Record 2"]);
    }
    let private = list(RecordFilter {
        scopes: vec![RecordScope::Task(task_id), RecordScope::User("u-1".into())],
        ..RecordFilter::new(fx.org.id, 100)
    })
    .await;
    assert_eq!(titles(&private), ["Record 5", "Record 4"]);

    // Pages.
    let mut seen = Vec::new();
    let mut filter = RecordFilter::new(fx.org.id, 4);
    loop {
        let page = list(filter.clone()).await;
        let Some(last) = page.last() else { break };
        filter.before = Some(RecordCursor::after(last));
        seen.extend(titles(&page));
    }
    assert_eq!(seen, titles(&everything));

    // Another tenant sees nothing; bad limits and user keys are rejected.
    let other = fixture(&mut c, "other").await;
    assert!(list(RecordFilter::new(other.org.id, 100)).await.is_empty());
    for bad in [
        RecordFilter::new(fx.org.id, 0),
        RecordFilter::new(fx.org.id, MAX_RECORDS_LISTED + 1),
        RecordFilter {
            scopes: vec![RecordScope::User(String::new())],
            ..RecordFilter::new(fx.org.id, 10)
        },
    ] {
        assert!(matches!(
            list_records(&mut c, &bad).await,
            Err(StoreError::InvalidInput(_))
        ));
    }

    // Deleting a project or task deletes the records scoped to it.
    hierarchy::delete_project(&mut c, web.id).await.unwrap();
    tasks::delete_task(&mut c, task_id).await.unwrap();
    assert_eq!(list(RecordFilter::new(fx.org.id, 100)).await.len(), 4);
}

#[tokio::test]
async fn full_text_search_ranks_and_filters() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let title_hit = insert_record(
        &mut c,
        &record(
            &fx,
            RecordScope::Organization,
            "payments.idempotency",
            "Idempotency keys for payments",
            "Retries must reuse the key.",
        ),
    )
    .await
    .unwrap();
    let body_hit = insert_record(
        &mut c,
        &record(
            &fx,
            RecordScope::Project(fx.project.id),
            "orders.retry",
            "Order retries",
            "The order client sends an idempotency key on retry.",
        ),
    )
    .await
    .unwrap();
    insert_record(
        &mut c,
        &record(
            &fx,
            RecordScope::Organization,
            "release.tags",
            "Release tags",
            "Tags are signed.",
        ),
    )
    .await
    .unwrap();

    let filter = RecordFilter::new(fx.org.id, 10);
    let hits = search_records(&mut c, &filter, "idempotency")
        .await
        .unwrap();
    assert_eq!(
        hits.iter().map(|h| h.record.id).collect::<Vec<_>>(),
        [title_hit.id, body_hit.id],
        "title and subject weigh more than the body"
    );
    assert!(hits[0].rank > hits[1].rank);
    // Subject segments are searchable; exclusions and phrases work.
    let hits = search_records(&mut c, &filter, "idempotency -order")
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    let hits = search_records(&mut c, &filter, "\"idempotency key\"")
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].record.id, body_hit.id);
    // Filters apply.
    let scoped = RecordFilter {
        scopes: vec![RecordScope::Project(fx.project.id)],
        ..RecordFilter::new(fx.org.id, 10)
    };
    let hits = search_records(&mut c, &scoped, "idempotency")
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert!(
        search_records(&mut c, &filter, "nothing-matches-this")
            .await
            .unwrap()
            .is_empty()
    );
    for bad in ["", "   ", "a\0b"] {
        assert!(matches!(
            search_records(&mut c, &filter, bad).await,
            Err(StoreError::InvalidInput(_))
        ));
    }
    let paged = RecordFilter {
        before: Some(RecordCursor::after(&title_hit)),
        ..filter.clone()
    };
    assert!(matches!(
        search_records(&mut c, &paged, "idempotency").await,
        Err(StoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn staleness_lookups_use_current_evidence_and_symbols() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let mut new = record(
        &fx,
        RecordScope::Project(fx.project.id),
        "payments.flow",
        "Payment flow",
        "Body",
    );
    new.state = KnowledgeState::Accepted;
    new.evidence = vec![evidence(&fx, "src/pay.rs", "old")];
    new.related_symbols = vec!["shop::pay".into()];
    let v1 = insert_record(&mut c, &new).await.unwrap();
    let mut other = record(&fx, RecordScope::Organization, "misc", "Misc", "Body");
    other.evidence = vec![evidence(&fx, "src/other.rs", "x")];
    let misc = insert_record(&mut c, &other).await.unwrap();

    let change = |content: &str| EvidenceChange {
        project: fx.project.id,
        path: path("src/pay.rs"),
        old_hash: hash(content),
    };
    let citing = records_citing_files(&mut c, fx.org.id, &[change("old")], &[])
        .await
        .unwrap();
    assert_eq!(citing.iter().map(|r| r.id).collect::<Vec<_>>(), [v1.id]);
    assert_eq!(citing[0].evidence, new.evidence);
    // States filter; unrelated hashes and other tenants match nothing.
    assert!(
        records_citing_files(
            &mut c,
            fx.org.id,
            &[change("old")],
            &[KnowledgeState::Proposed]
        )
        .await
        .unwrap()
        .is_empty()
    );
    assert!(
        records_citing_files(&mut c, fx.org.id, &[change("other")], &[])
            .await
            .unwrap()
            .is_empty()
    );
    let tenant = fixture(&mut c, "tenant").await;
    assert!(
        records_citing_files(&mut c, tenant.org.id, &[change("old")], &[])
            .await
            .unwrap()
            .is_empty()
    );
    assert!(
        records_citing_files(&mut c, fx.org.id, &[], &[])
            .await
            .unwrap()
            .is_empty()
    );

    // After revalidation with new evidence only the new hash matches.
    let revalidated = RecordUpdate {
        content: Some(RecordContent {
            title: v1.title.clone(),
            body: v1.body.clone(),
            tags: Vec::new(),
            evidence: vec![evidence(&fx, "src/pay.rs", "new")],
        }),
        ..state_change(&v1, KnowledgeState::Accepted, t(1))
    };
    update_record(&mut c, &revalidated).await.unwrap();
    assert!(
        records_citing_files(&mut c, fx.org.id, &[change("old")], &[])
            .await
            .unwrap()
            .is_empty()
    );
    let both = records_citing_files(
        &mut c,
        fx.org.id,
        &[
            change("new"),
            EvidenceChange {
                project: fx.project.id,
                path: path("src/other.rs"),
                old_hash: hash("x"),
            },
        ],
        &[],
    )
    .await
    .unwrap();
    assert_eq!(
        both.iter().map(|r| r.id).collect::<BTreeSet<_>>(),
        BTreeSet::from([v1.id, misc.id])
    );

    let about = records_about_symbols(&mut c, fx.org.id, &["shop::pay".into()], &[])
        .await
        .unwrap();
    assert_eq!(about.iter().map(|r| r.id).collect::<Vec<_>>(), [v1.id]);
    assert!(
        records_about_symbols(&mut c, fx.org.id, &["nope".into()], &[])
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn records_stay_inside_their_tenant() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let foreign = fixture(&mut c, "foreign").await;
    let foreign_record = insert_record(
        &mut c,
        &record(&foreign, RecordScope::Organization, "x", "Foreign", "Body"),
    )
    .await
    .unwrap();

    let cases = [
        record(&fx, RecordScope::Project(foreign.project.id), "x", "T", "B"),
        record(
            &fx,
            RecordScope::Workspace(foreign.workspace.id),
            "x",
            "T",
            "B",
        ),
        record(
            &fx,
            RecordScope::Task(TaskId(Uuid::now_v7())),
            "x",
            "T",
            "B",
        ),
        NewRecord {
            superseded_by: Some(foreign_record.id),
            ..record(&fx, RecordScope::Organization, "x", "T", "B")
        },
        NewRecord {
            evidence: vec![evidence(&foreign, "src/a.rs", "a")],
            ..record(&fx, RecordScope::Organization, "x", "T", "B")
        },
    ];
    for case in cases {
        let err = insert_record(&mut c, &case).await.unwrap_err();
        assert!(matches!(err, StoreError::NotFound { .. }), "{err}");
        // Nothing of the failed write remains.
        assert_eq!(get_record(&mut c, case.id).await.unwrap(), None);
    }

    let invalid = [
        NewRecord {
            subject: "Not Canonical".into(),
            ..record(&fx, RecordScope::Organization, "x", "T", "B")
        },
        record(&fx, RecordScope::Organization, "x", "", "B"),
        record(&fx, RecordScope::User(" padded".into()), "x", "T", "B"),
        NewRecord {
            version: 0,
            ..record(&fx, RecordScope::Organization, "x", "T", "B")
        },
        NewRecord {
            evidence: vec![RecordEvidence {
                commit: "NOTHEX!".into(),
                ..evidence(&fx, "src/a.rs", "a")
            }],
            ..record(&fx, RecordScope::Organization, "x", "T", "B")
        },
        NewRecord {
            history: vec![HistoryEntry {
                reason: String::new(),
                ..entry(
                    T0,
                    KnowledgeAction::Propose,
                    None,
                    KnowledgeState::Proposed,
                    1,
                )
            }],
            ..record(&fx, RecordScope::Organization, "x", "T", "B")
        },
    ];
    for case in invalid {
        let err = insert_record(&mut c, &case).await.unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
    }
}
