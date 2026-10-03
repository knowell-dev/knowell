use knowell_store::StoreError;
use knowell_store::usage::*;
use time::OffsetDateTime;
use time::macros::datetime;

use crate::common::{fixture, require_db};

const HOUR: OffsetDateTime = datetime!(2026-10-03 10:00 UTC);

/// One recorded call of `tool` by `agent` taking `latency_ms`.
fn one_call(tool: &str, agent: &str, latency_ms: u64, ok: bool, at: OffsetDateTime) -> ToolUsage {
    let mut latency_buckets = vec![0; LATENCY_BUCKETS];
    latency_buckets[latency_bucket(latency_ms)] = 1;
    ToolUsage {
        hour: hour_of(at).unwrap(),
        tool: tool.into(),
        agent: agent.into(),
        calls: 1,
        errors: u64::from(!ok),
        tokens_returned: 100,
        latency_ms_sum: latency_ms,
        latency_buckets,
        last_call_at: at,
    }
}

#[tokio::test]
async fn usage_hours_accumulate_per_organization_and_prune() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let other = fixture(&mut c, "other").await;
    assert_eq!(last_tool_call(&mut c, fx.org.id).await.unwrap(), None);

    let early = HOUR + time::Duration::minutes(5);
    let late = HOUR + time::Duration::minutes(50);
    record_tool_usage(
        &mut c,
        fx.org.id,
        &[
            one_call("search", "claude-code", 12, true, early),
            one_call("read_file", "claude-code", 2, true, early),
        ],
    )
    .await
    .unwrap();
    record_tool_usage(
        &mut c,
        fx.org.id,
        &[one_call("search", "claude-code", 900, false, late)],
    )
    .await
    .unwrap();
    record_tool_usage(
        &mut c,
        other.org.id,
        &[one_call("search", "codex", 5, true, late)],
    )
    .await
    .unwrap();

    let rows = tool_usage(&mut c, fx.org.id, HOUR).await.unwrap();
    assert_eq!(
        rows.iter().map(|r| r.tool.as_str()).collect::<Vec<_>>(),
        ["read_file", "search"]
    );
    let search = &rows[1];
    assert_eq!(search.calls, 2);
    assert_eq!(search.errors, 1);
    assert_eq!(search.tokens_returned, 200);
    assert_eq!(search.latency_ms_sum, 912);
    assert_eq!(search.latency_buckets[latency_bucket(12)], 1);
    assert_eq!(search.latency_buckets[latency_bucket(900)], 1);
    assert_eq!(search.latency_buckets.iter().sum::<u64>(), 2);
    assert_eq!(search.last_call_at, late);
    assert_eq!(last_tool_call(&mut c, fx.org.id).await.unwrap(), Some(late));
    // The other organization's call is not visible here, nor ours there.
    let theirs = tool_usage(&mut c, other.org.id, HOUR).await.unwrap();
    assert_eq!(theirs.len(), 1);
    assert_eq!(theirs[0].agent, "codex");
    // A later period start excludes the hour.
    assert!(
        tool_usage(&mut c, fx.org.id, HOUR + time::Duration::hours(1))
            .await
            .unwrap()
            .is_empty()
    );

    assert_eq!(
        prune_tool_usage(&mut c, fx.org.id, HOUR).await.unwrap(),
        0,
        "the cut-off keeps the hour that starts at it"
    );
    assert_eq!(
        prune_tool_usage(&mut c, fx.org.id, HOUR + time::Duration::hours(1))
            .await
            .unwrap(),
        2
    );
    assert!(
        tool_usage(&mut c, fx.org.id, HOUR)
            .await
            .unwrap()
            .is_empty()
    );
    assert_eq!(
        tool_usage(&mut c, other.org.id, HOUR).await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn concurrent_flushes_sum_exactly() {
    const FLUSHERS: u64 = 8;
    const CALLS: u64 = 25;
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let mut handles = Vec::new();
    for flusher in 0..FLUSHERS {
        let store = db.store.clone();
        let org = fx.org.id;
        handles.push(tokio::spawn(async move {
            let mut c = store.acquire().await.unwrap();
            for call in 0..CALLS {
                let at = HOUR + time::Duration::seconds(i64::try_from(call).unwrap());
                // Two rows per flush, in different orders, to exercise locking.
                let mut batch = vec![
                    one_call("search", "claude-code", flusher + 1, true, at),
                    one_call("trace_flow", "claude-code", call, call % 5 != 0, at),
                ];
                if flusher % 2 == 1 {
                    batch.reverse();
                }
                record_tool_usage(&mut c, org, &batch).await.unwrap();
            }
        }));
    }
    for handle in handles {
        handle.await.unwrap();
    }
    let rows = tool_usage(&mut c, fx.org.id, HOUR).await.unwrap();
    assert_eq!(rows.len(), 2);
    for row in &rows {
        assert_eq!(row.calls, FLUSHERS * CALLS, "{}", row.tool);
        assert_eq!(row.latency_buckets.iter().sum::<u64>(), FLUSHERS * CALLS);
        assert_eq!(row.tokens_returned, FLUSHERS * CALLS * 100);
    }
    let trace = rows.iter().find(|r| r.tool == "trace_flow").unwrap();
    assert_eq!(trace.errors, FLUSHERS * (CALLS / 5));
}

#[tokio::test]
async fn invalid_usage_is_rejected_without_echo_or_partial_writes() {
    let db = require_db!();
    let mut c = db.conn().await;
    let fx = fixture(&mut c, "shop").await;
    let canary = "KNOWELL_CANARY_usage_label";
    let good = one_call("search", "claude-code", 3, true, HOUR);
    let invalid = [
        ToolUsage {
            agent: format!("{canary} with spaces"),
            ..good.clone()
        },
        ToolUsage {
            agent: format!("{canary}\u{0}"),
            ..good.clone()
        },
        ToolUsage {
            agent: "a".repeat(MAX_USAGE_LABEL_BYTES + 1),
            ..good.clone()
        },
        ToolUsage {
            agent: String::new(),
            ..good.clone()
        },
        ToolUsage {
            tool: canary.to_owned(),
            ..good.clone()
        },
        ToolUsage {
            calls: 0,
            latency_buckets: vec![0; LATENCY_BUCKETS],
            ..good.clone()
        },
        ToolUsage {
            errors: 2,
            ..good.clone()
        },
        ToolUsage {
            latency_buckets: vec![0; LATENCY_BUCKETS - 1],
            ..good.clone()
        },
        ToolUsage {
            calls: 2,
            ..good.clone()
        },
        ToolUsage {
            hour: HOUR + time::Duration::minutes(1),
            ..good.clone()
        },
    ];
    for bad in invalid {
        // A valid row in the same batch is not written either.
        let err = record_tool_usage(&mut c, fx.org.id, &[good.clone(), bad])
            .await
            .unwrap_err();
        assert!(matches!(err, StoreError::InvalidInput(_)), "{err}");
        assert!(!err.to_string().contains(canary), "{err}");
    }
    assert!(
        tool_usage(&mut c, fx.org.id, HOUR)
            .await
            .unwrap()
            .is_empty()
    );
}
