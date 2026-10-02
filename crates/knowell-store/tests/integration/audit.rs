use knowell_store::audit::*;
use knowell_store::{PrincipalId, StoreError, hierarchy};
use time::OffsetDateTime;
use time::macros::datetime;
use uuid::Uuid;

use crate::common::{fixture, require_db};

const T0: OffsetDateTime = datetime!(2026-10-02 12:00 UTC);

fn entry(minute: i64, allowed: bool) -> NewAuditEntry {
    NewAuditEntry {
        organization: None,
        at: T0 + time::Duration::minutes(minute),
        actor: "user:00000000-0000-0000-0000-000000000001".into(),
        principal: Some(PrincipalId(Uuid::from_u128(1))),
        action: "manage_index".into(),
        resource: "project:main/api".into(),
        allowed,
        reason: if allowed {
            "granted"
        } else {
            "denied_no_grant"
        }
        .into(),
        request_id: format!("0190d1c4-0000-7000-8000-{minute:012}"),
    }
}

#[tokio::test]
async fn entries_append_list_and_page() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let mut batch = Vec::new();
    for minute in 0..5 {
        batch.push(NewAuditEntry {
            organization: Some(fx.org.id),
            ..entry(minute, minute % 2 == 0)
        });
    }
    batch.push(NewAuditEntry {
        actor: "service_account:00000000-0000-0000-0000-000000000002".into(),
        principal: Some(PrincipalId(Uuid::from_u128(2))),
        action: "read_audit".into(),
        resource: "org".into(),
        ..entry(10, true)
    });
    assert_eq!(append_audit(&mut c, &batch).await.unwrap(), 6);
    assert_eq!(append_audit(&mut c, &[]).await.unwrap(), 0);

    let all = list_audit(&mut c, &AuditFilter::new(100)).await.unwrap();
    assert_eq!(all.len(), 6);
    assert_eq!(all[0].at, T0 + time::Duration::minutes(10));
    assert_eq!(all[0].organization, None);
    assert_eq!(all[1].request_id, batch[4].request_id);
    assert_eq!(all[1].organization, Some(fx.org.id));

    let filtered = |filter: AuditFilter| {
        let store = db.store.clone();
        async move {
            let mut c = store.acquire().await.unwrap();
            list_audit(&mut c, &filter).await.unwrap()
        }
    };
    assert_eq!(
        filtered(AuditFilter {
            organization: Some(fx.org.id),
            ..AuditFilter::new(100)
        })
        .await
        .len(),
        5
    );
    let denied = filtered(AuditFilter {
        allowed: Some(false),
        ..AuditFilter::new(100)
    })
    .await;
    assert_eq!(denied.len(), 2);
    assert!(denied.iter().all(|e| e.reason == "denied_no_grant"));
    assert_eq!(
        filtered(AuditFilter {
            principal: Some(PrincipalId(Uuid::from_u128(2))),
            ..AuditFilter::new(100)
        })
        .await
        .len(),
        1
    );
    assert_eq!(
        filtered(AuditFilter {
            action: Some("read_audit".into()),
            ..AuditFilter::new(100)
        })
        .await
        .len(),
        1
    );
    assert_eq!(
        filtered(AuditFilter {
            since: Some(T0 + time::Duration::minutes(3)),
            ..AuditFilter::new(100)
        })
        .await
        .len(),
        3
    );
    let mut seen = Vec::new();
    let mut filter = AuditFilter::new(4);
    loop {
        let page = list_audit(&mut c, &filter).await.unwrap();
        let Some(last) = page.last() else { break };
        filter.before = Some(AuditCursor::after(last));
        seen.extend(page.into_iter().map(|e| e.id));
    }
    assert_eq!(seen, all.iter().map(|e| e.id).collect::<Vec<_>>());
    assert!(matches!(
        list_audit(&mut c, &AuditFilter::new(0)).await,
        Err(StoreError::InvalidInput(_))
    ));
}

#[tokio::test]
async fn free_text_is_refused_and_nothing_is_written() {
    let db = require_db!();
    let mut c = db.conn().await;
    let canary = "KNOWELL_CANARY token value";
    for bad in [
        NewAuditEntry {
            resource: canary.into(),
            ..entry(1, true)
        },
        NewAuditEntry {
            request_id: "has space".into(),
            ..entry(1, true)
        },
        NewAuditEntry {
            reason: "Granted!".into(),
            ..entry(1, true)
        },
        NewAuditEntry {
            actor: String::new(),
            ..entry(1, true)
        },
    ] {
        // One bad entry fails the whole batch.
        let err = append_audit(&mut c, &[entry(0, true), bad])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)));
        assert!(!err.to_string().contains("CANARY"));
    }
    assert!(
        list_audit(&mut c, &AuditFilter::new(10))
            .await
            .unwrap()
            .is_empty()
    );
}

#[tokio::test]
async fn the_log_is_append_only_except_for_retention() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let other = fixture(&mut c, "other").await;
    let mut batch = Vec::new();
    for minute in 0..4 {
        batch.push(NewAuditEntry {
            organization: Some(fx.org.id),
            ..entry(minute, true)
        });
    }
    batch.push(NewAuditEntry {
        organization: Some(other.org.id),
        ..entry(0, false)
    });
    append_audit(&mut c, &batch).await.unwrap();
    for sql in [
        "UPDATE audit_log SET allowed = NOT allowed",
        "DELETE FROM audit_log",
        "TRUNCATE audit_log",
    ] {
        assert!(sqlx::query(sql).execute(&mut *c).await.is_err(), "{sql}");
    }

    let cutoff = T0 + time::Duration::minutes(2);
    assert_eq!(
        prune_audit_log(&mut c, Some(fx.org.id), cutoff)
            .await
            .unwrap(),
        2
    );
    // The guard is closed again on the same connection.
    assert!(
        sqlx::query("DELETE FROM audit_log")
            .execute(&mut *c)
            .await
            .is_err()
    );
    let left = list_audit(&mut c, &AuditFilter::new(10)).await.unwrap();
    assert_eq!(left.len(), 3);

    // The log outlives the organization it describes.
    hierarchy::delete_organization(&mut c, other.org.id)
        .await
        .unwrap();
    assert_eq!(
        list_audit(
            &mut c,
            &AuditFilter {
                organization: Some(other.org.id),
                ..AuditFilter::new(10)
            }
        )
        .await
        .unwrap()
        .len(),
        1
    );
    assert_eq!(
        prune_audit_log(&mut c, None, T0 + time::Duration::days(1))
            .await
            .unwrap(),
        3
    );
}
