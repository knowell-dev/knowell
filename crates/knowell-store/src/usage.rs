//! Durable MCP tool usage, summed per organization, UTC hour, tool and agent.
//!
//! - [`record_tool_usage`] adds buffered counts in one transaction. Additions
//!   are commutative sums, so several engines may flush into the same rows.
//! - [`tool_usage`] reads the hourly rows of a period; [`last_tool_call`] the
//!   newest call; [`prune_tool_usage`] drops hours before a cut-off.
//! - Latencies are a histogram: [`latency_bucket`] places a duration and
//!   [`latency_bucket_bound_ms`] reports a bucket, so percentiles computed from
//!   persisted rows have a known resolution instead of exact samples.

use sqlx::{Connection, PgConnection};
use time::{OffsetDateTime, Time, UtcOffset};

use crate::error::StoreError;
use crate::ids::OrganizationId;
use crate::types::{from_i64, to_i64};

/// Number of latency histogram buckets.
pub const LATENCY_BUCKETS: usize = 96;
/// Longest tool name or agent label, in bytes.
pub const MAX_USAGE_LABEL_BYTES: usize = 64;

/// The histogram bucket of a latency in milliseconds: exact below 4 ms, then
/// four buckets per power of two (each at most 25 % wide). Latencies from
/// 7 * 2^22 ms (about 8.2 hours) on share the last bucket.
pub fn latency_bucket(ms: u64) -> usize {
    if ms < 4 {
        return usize::try_from(ms).unwrap_or(0);
    }
    // floor(log2(ms)) >= 2 here.
    let octave = u64::from(63 - ms.leading_zeros());
    let sub = (ms >> (octave - 2)) & 3;
    let index = 4 + (octave - 2) * 4 + sub;
    usize::try_from(index)
        .unwrap_or(LATENCY_BUCKETS - 1)
        .min(LATENCY_BUCKETS - 1)
}

/// The largest latency (ms) of bucket `index`; for the last, open-ended
/// bucket its smallest latency instead. `None` for an index out of range.
pub fn latency_bucket_bound_ms(index: usize) -> Option<u64> {
    if index >= LATENCY_BUCKETS {
        return None;
    }
    let index = u64::try_from(index).ok()?;
    if index < 4 {
        return Some(index);
    }
    let octave = (index - 4) / 4 + 2;
    let sub = (index - 4) % 4;
    let shift = u32::try_from(octave - 2).ok()?;
    let lower = (4 + sub).checked_shl(shift)?;
    if usize::try_from(index).ok()? == LATENCY_BUCKETS - 1 {
        return Some(lower);
    }
    (5 + sub).checked_shl(shift).map(|next| next - 1)
}

/// The start of the UTC hour containing `at`.
pub fn hour_of(at: OffsetDateTime) -> Result<OffsetDateTime, StoreError> {
    let utc = at.to_offset(UtcOffset::UTC);
    let start = Time::from_hms(utc.hour(), 0, 0)
        .map_err(|_| StoreError::invalid("usage hour is out of range"))?;
    Ok(utc.replace_time(start))
}

/// Usage of one tool by one agent in one UTC hour: an addition when written,
/// the stored sum when read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolUsage {
    /// Start of the UTC hour.
    pub hour: OffsetDateTime,
    /// MCP tool name (`[a-z][a-z0-9_]*`, at most 64 bytes).
    pub tool: String,
    /// Agent label (`[A-Za-z0-9._-]`, 1 to 64 bytes); see [`agent_label`].
    pub agent: String,
    /// Calls, at least 1.
    pub calls: u64,
    /// Calls that failed, at most `calls`.
    pub errors: u64,
    /// Estimated tokens returned (four bytes of JSON per token).
    pub tokens_returned: u64,
    /// Sum of call latencies, in milliseconds.
    pub latency_ms_sum: u64,
    /// Calls per [`latency_bucket`]; [`LATENCY_BUCKETS`] entries summing to
    /// `calls`.
    pub latency_buckets: Vec<u64>,
    /// When the newest of these calls finished.
    pub last_call_at: OffsetDateTime,
}

/// A storable agent label for an untrusted client name: characters outside
/// `[A-Za-z0-9._-]` become `_`, the label is cut to 64 bytes, and an empty
/// name becomes `mcp-client`.
pub fn agent_label(client: &str) -> String {
    let label: String = client
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-') {
                c
            } else {
                '_'
            }
        })
        .take(MAX_USAGE_LABEL_BYTES)
        .collect();
    if label.is_empty() {
        "mcp-client".to_owned()
    } else {
        label
    }
}

fn check_usage(usage: &ToolUsage) -> Result<(), StoreError> {
    let tool_ok = usage.tool.len() <= MAX_USAGE_LABEL_BYTES
        && usage
            .tool
            .chars()
            .next()
            .is_some_and(|c| c.is_ascii_lowercase())
        && usage
            .tool
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_');
    if !tool_ok {
        return Err(StoreError::invalid(
            "tool usage names a tool outside [a-z][a-z0-9_]{0,63}",
        ));
    }
    if usage.agent.is_empty() || agent_label(&usage.agent) != usage.agent {
        return Err(StoreError::invalid(
            "tool usage agent labels must be 1-64 characters of A-Z, a-z, 0-9, '.', '_' or '-'",
        ));
    }
    if usage.calls == 0 || usage.errors > usage.calls {
        return Err(StoreError::invalid(
            "tool usage needs at least one call and no more errors than calls",
        ));
    }
    if usage.latency_buckets.len() != LATENCY_BUCKETS {
        return Err(StoreError::invalid(format!(
            "tool usage needs {LATENCY_BUCKETS} latency buckets"
        )));
    }
    let bucketed = usage
        .latency_buckets
        .iter()
        .try_fold(0u64, |sum, n| sum.checked_add(*n));
    if bucketed != Some(usage.calls) {
        return Err(StoreError::invalid(
            "tool usage latency buckets must sum to its calls",
        ));
    }
    if hour_of(usage.hour)? != usage.hour {
        return Err(StoreError::invalid(
            "tool usage hours must start a UTC hour",
        ));
    }
    Ok(())
}

/// Adds `usage` to the stored sums of `organization` in one transaction:
/// either every row is added or none. Rows are written in key order so that
/// concurrent flushes cannot deadlock. Invalid input is
/// [`StoreError::InvalidInput`] (the message never repeats a value).
pub async fn record_tool_usage(
    conn: &mut PgConnection,
    organization: OrganizationId,
    usage: &[ToolUsage],
) -> Result<(), StoreError> {
    usage.iter().try_for_each(check_usage)?;
    let mut ordered: Vec<&ToolUsage> = usage.iter().collect();
    ordered.sort_by(|a, b| (a.hour, &a.tool, &a.agent).cmp(&(b.hour, &b.tool, &b.agent)));
    let mut tx = conn.begin().await?;
    for row in ordered {
        let buckets = row
            .latency_buckets
            .iter()
            .map(|n| to_i64(*n, "latency bucket"))
            .collect::<Result<Vec<i64>, _>>()?;
        sqlx::query(
            "INSERT INTO tool_usage_hour (organization_id, hour, tool, agent, calls, errors,
                 tokens_returned, latency_ms_sum, latency_buckets, last_call_at)
             VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10)
             ON CONFLICT (organization_id, hour, tool, agent) DO UPDATE SET
               calls = tool_usage_hour.calls + EXCLUDED.calls,
               errors = tool_usage_hour.errors + EXCLUDED.errors,
               tokens_returned = tool_usage_hour.tokens_returned + EXCLUDED.tokens_returned,
               latency_ms_sum = tool_usage_hour.latency_ms_sum + EXCLUDED.latency_ms_sum,
               latency_buckets = ARRAY(
                 SELECT a + b
                 FROM unnest(tool_usage_hour.latency_buckets, EXCLUDED.latency_buckets)
                   WITH ORDINALITY AS u(a, b, i)
                 ORDER BY i),
               last_call_at = greatest(tool_usage_hour.last_call_at, EXCLUDED.last_call_at)",
        )
        .bind(organization)
        .bind(row.hour)
        .bind(&row.tool)
        .bind(&row.agent)
        .bind(to_i64(row.calls, "tool usage calls")?)
        .bind(to_i64(row.errors, "tool usage errors")?)
        .bind(to_i64(row.tokens_returned, "tool usage tokens")?)
        .bind(to_i64(row.latency_ms_sum, "tool usage latency")?)
        .bind(&buckets)
        .bind(row.last_call_at)
        .execute(&mut *tx)
        .await?;
    }
    tx.commit().await?;
    Ok(())
}

#[derive(sqlx::FromRow)]
struct UsageRow {
    hour: OffsetDateTime,
    tool: String,
    agent: String,
    calls: i64,
    errors: i64,
    tokens_returned: i64,
    latency_ms_sum: i64,
    latency_buckets: Vec<i64>,
    last_call_at: OffsetDateTime,
}

impl TryFrom<UsageRow> for ToolUsage {
    type Error = StoreError;

    fn try_from(row: UsageRow) -> Result<Self, StoreError> {
        Ok(ToolUsage {
            hour: row.hour,
            tool: row.tool,
            agent: row.agent,
            calls: from_i64(row.calls, "tool usage calls")?,
            errors: from_i64(row.errors, "tool usage errors")?,
            tokens_returned: from_i64(row.tokens_returned, "tool usage tokens")?,
            latency_ms_sum: from_i64(row.latency_ms_sum, "tool usage latency")?,
            latency_buckets: row
                .latency_buckets
                .into_iter()
                .map(|n| from_i64(n, "latency bucket"))
                .collect::<Result<_, _>>()?,
            last_call_at: row.last_call_at,
        })
    }
}

/// The hourly rows of `organization` from the hour containing `since` on,
/// ordered by hour, tool and agent.
pub async fn tool_usage(
    conn: &mut PgConnection,
    organization: OrganizationId,
    since: OffsetDateTime,
) -> Result<Vec<ToolUsage>, StoreError> {
    let rows = sqlx::query_as::<_, UsageRow>(
        "SELECT hour, tool, agent, calls, errors, tokens_returned, latency_ms_sum,
                latency_buckets, last_call_at
         FROM tool_usage_hour
         WHERE organization_id = $1 AND hour >= $2
         ORDER BY hour, tool, agent",
    )
    .bind(organization)
    .bind(hour_of(since)?)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// When the newest recorded tool call of `organization` finished.
pub async fn last_tool_call(
    conn: &mut PgConnection,
    organization: OrganizationId,
) -> Result<Option<OffsetDateTime>, StoreError> {
    Ok(sqlx::query_scalar(
        "SELECT max(last_call_at) FROM tool_usage_hour WHERE organization_id = $1",
    )
    .bind(organization)
    .fetch_one(conn)
    .await?)
}

/// Deletes the hours of `organization` that start before `before`; returns
/// how many rows went.
pub async fn prune_tool_usage(
    conn: &mut PgConnection,
    organization: OrganizationId,
    before: OffsetDateTime,
) -> Result<u64, StoreError> {
    let done = sqlx::query("DELETE FROM tool_usage_hour WHERE organization_id = $1 AND hour < $2")
        .bind(organization)
        .bind(before)
        .execute(conn)
        .await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn latency_buckets_are_ordered_and_bounded() {
        assert_eq!(latency_bucket(0), 0);
        assert_eq!(latency_bucket(3), 3);
        assert_eq!(latency_bucket(4), 4);
        assert_eq!(latency_bucket(u64::MAX), LATENCY_BUCKETS - 1);
        let mut previous = 0;
        for ms in 0..100_000u64 {
            let bucket = latency_bucket(ms);
            assert!(bucket >= previous, "buckets never decrease");
            previous = bucket;
            let bound = latency_bucket_bound_ms(bucket).unwrap();
            assert!(ms <= bound, "{ms} ms exceeds its bucket bound {bound}");
            // Each bucket is at most a quarter of its lower end wide.
            assert!(
                bound - ms <= ms / 4,
                "{ms} ms in a bucket ending at {bound}"
            );
        }
        assert_eq!(latency_bucket_bound_ms(LATENCY_BUCKETS), None);
        let last = latency_bucket_bound_ms(LATENCY_BUCKETS - 1).unwrap();
        assert_eq!(latency_bucket(last), LATENCY_BUCKETS - 1);
        assert_eq!(latency_bucket(last - 1), LATENCY_BUCKETS - 2);
    }

    #[test]
    fn hours_start_in_utc() {
        let at = time::macros::datetime!(2026-10-03 14:59:59.9 +03:00);
        assert_eq!(
            hour_of(at).unwrap(),
            time::macros::datetime!(2026-10-03 11:00 UTC)
        );
    }

    #[test]
    fn agent_labels_are_sanitized() {
        assert_eq!(agent_label("claude-code"), "claude-code");
        assert_eq!(agent_label("Codex 1.0/beta"), "Codex_1.0_beta");
        assert_eq!(agent_label(""), "mcp-client");
        assert_eq!(agent_label(&"a".repeat(100)).len(), MAX_USAGE_LABEL_BYTES);
        assert_eq!(agent_label("ajan\u{0}\n"), "ajan__");
    }
}
