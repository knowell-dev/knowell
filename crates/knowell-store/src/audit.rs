//! The append-only audit log of authorization outcomes.
//!
//! Entries hold identifiers, codes and the request id only: every text
//! column accepts a small alphabet (`[A-Za-z0-9._:/@-]`; reasons
//! `[a-z0-9_]`), so free text — and with it any secret — cannot be written.
//! The table rejects UPDATE, DELETE and TRUNCATE; [`prune_audit_log`] is the
//! only way rows leave it (retention). Entries are not deleted with their
//! organization: the log outlives what it describes.

use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;

use crate::error::StoreError;
use crate::ids::{AuditEntryId, OrganizationId, PrincipalId};

/// Longest actor or resource text, in bytes.
pub const MAX_AUDIT_TEXT_BYTES: usize = 512;
/// Longest action text, in bytes.
pub const MAX_AUDIT_ACTION_BYTES: usize = 256;
/// Longest reason code, in bytes.
pub const MAX_AUDIT_REASON_BYTES: usize = 64;
/// Longest request id, in bytes.
pub const MAX_AUDIT_REQUEST_ID_BYTES: usize = 128;
/// Most entries one [`append_audit`] call writes.
pub const MAX_AUDIT_BATCH: usize = 1000;
/// Most entries one [`list_audit`] call returns.
pub const MAX_AUDIT_LISTED: u32 = 1000;

/// An entry to append.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewAuditEntry {
    /// The organization; `None` for events before it existed.
    pub organization: Option<OrganizationId>,
    /// When the decision was made.
    pub at: OffsetDateTime,
    /// Who acted, in knowell-auth text form (`user:<uuid>`, …).
    pub actor: String,
    /// The acting principal (an agent's user), when it is one.
    pub principal: Option<PrincipalId>,
    /// The action code (`manage_index`, …).
    pub action: String,
    /// The resource text (`org`, `workspace:<w>`, `project:<w>/<p>`).
    pub resource: String,
    /// Whether it was allowed.
    pub allowed: bool,
    /// The decision reason code (`granted`, `denied_no_grant`, …).
    pub reason: String,
    /// Correlation id of the request.
    pub request_id: String,
}

/// A stored entry.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditEntry {
    /// Id (UUIDv7: sorts by write time).
    pub id: AuditEntryId,
    /// The organization.
    pub organization: Option<OrganizationId>,
    /// When the decision was made.
    pub at: OffsetDateTime,
    /// Who acted.
    pub actor: String,
    /// The acting principal.
    pub principal: Option<PrincipalId>,
    /// The action code.
    pub action: String,
    /// The resource text.
    pub resource: String,
    /// Whether it was allowed.
    pub allowed: bool,
    /// The decision reason code.
    pub reason: String,
    /// Correlation id of the request.
    pub request_id: String,
    /// When the row was written (server clock).
    pub recorded_at: OffsetDateTime,
}

/// Keyset position for paging through [`list_audit`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AuditCursor {
    /// `at` of the last entry seen.
    pub at: OffsetDateTime,
    /// Id of the last entry seen (tie-break).
    pub id: AuditEntryId,
}

impl AuditCursor {
    /// The cursor after `entry` (pass the last entry of a page).
    pub fn after(entry: &AuditEntry) -> Self {
        Self {
            at: entry.at,
            id: entry.id,
        }
    }
}

/// What [`list_audit`] returns.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuditFilter {
    /// Entries of this organization; `None` = every entry.
    pub organization: Option<OrganizationId>,
    /// Entries of this acting principal.
    pub principal: Option<PrincipalId>,
    /// Only allowed (`Some(true)`) or denied (`Some(false)`) decisions.
    pub allowed: Option<bool>,
    /// Entries with this action code.
    pub action: Option<String>,
    /// Entries at or after this time.
    pub since: Option<OffsetDateTime>,
    /// Most entries to return, 1..=[`MAX_AUDIT_LISTED`].
    pub limit: u32,
    /// Continue after this position.
    pub before: Option<AuditCursor>,
}

impl AuditFilter {
    /// Every entry, newest first, at most `limit`.
    pub fn new(limit: u32) -> Self {
        Self {
            organization: None,
            principal: None,
            allowed: None,
            action: None,
            since: None,
            limit,
            before: None,
        }
    }
}

#[derive(sqlx::FromRow)]
struct AuditRow {
    id: AuditEntryId,
    organization_id: Option<OrganizationId>,
    at: OffsetDateTime,
    actor: String,
    principal_id: Option<PrincipalId>,
    action: String,
    resource: String,
    allowed: bool,
    reason: String,
    request_id: String,
    recorded_at: OffsetDateTime,
}

impl From<AuditRow> for AuditEntry {
    fn from(row: AuditRow) -> Self {
        Self {
            id: row.id,
            organization: row.organization_id,
            at: row.at,
            actor: row.actor,
            principal: row.principal_id,
            action: row.action,
            resource: row.resource,
            allowed: row.allowed,
            reason: row.reason,
            request_id: row.request_id,
            recorded_at: row.recorded_at,
        }
    }
}

/// Checks one audit text column against its alphabet and length. The error
/// names the field, never the value.
fn check_field(
    field: &str,
    text: &str,
    max: usize,
    allowed: fn(u8) -> bool,
) -> Result<(), StoreError> {
    if text.is_empty() || text.len() > max || !text.bytes().all(allowed) {
        return Err(StoreError::invalid(format!(
            "audit {field} must be 1-{max} bytes of its restricted alphabet"
        )));
    }
    Ok(())
}

fn ident_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"._:/@-".contains(&b)
}

fn reason_byte(b: u8) -> bool {
    b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_'
}

fn request_id_byte(b: u8) -> bool {
    b.is_ascii_alphanumeric() || b"._:-".contains(&b)
}

fn check_entry(entry: &NewAuditEntry) -> Result<(), StoreError> {
    check_field("actor", &entry.actor, MAX_AUDIT_TEXT_BYTES, ident_byte)?;
    check_field("action", &entry.action, MAX_AUDIT_ACTION_BYTES, ident_byte)?;
    check_field(
        "resource",
        &entry.resource,
        MAX_AUDIT_TEXT_BYTES,
        ident_byte,
    )?;
    check_field("reason", &entry.reason, MAX_AUDIT_REASON_BYTES, reason_byte)?;
    check_field(
        "request id",
        &entry.request_id,
        MAX_AUDIT_REQUEST_ID_BYTES,
        request_id_byte,
    )
}

/// Appends entries in one statement (all or nothing). Returns how many were
/// written. Fails with [`StoreError::InvalidInput`] when any entry breaks
/// the alphabet or length rules, before anything is written.
pub async fn append_audit(
    conn: &mut PgConnection,
    entries: &[NewAuditEntry],
) -> Result<u64, StoreError> {
    if entries.is_empty() {
        return Ok(0);
    }
    if entries.len() > MAX_AUDIT_BATCH {
        return Err(StoreError::invalid(format!(
            "at most {MAX_AUDIT_BATCH} audit entries per call"
        )));
    }
    entries.iter().try_for_each(check_entry)?;
    let n = entries.len();
    let (mut orgs, mut ats, mut actors, mut principals) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    let (mut actions, mut resources, mut allowed, mut reasons, mut requests) = (
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
        Vec::with_capacity(n),
    );
    for e in entries {
        orgs.push(e.organization);
        ats.push(e.at);
        actors.push(e.actor.as_str());
        principals.push(e.principal);
        actions.push(e.action.as_str());
        resources.push(e.resource.as_str());
        allowed.push(e.allowed);
        reasons.push(e.reason.as_str());
        requests.push(e.request_id.as_str());
    }
    let done = sqlx::query(
        "INSERT INTO audit_log (organization_id, at, actor, principal_id, action, resource,
                                allowed, reason, request_id)
         SELECT u.organization_id, u.at, u.actor, u.principal_id, u.action, u.resource,
                u.allowed, u.reason, u.request_id
         FROM unnest($1::uuid[], $2::timestamptz[], $3::text[], $4::uuid[], $5::text[],
                     $6::text[], $7::boolean[], $8::text[], $9::text[])
              WITH ORDINALITY AS u(organization_id, at, actor, principal_id, action, resource,
                                   allowed, reason, request_id, ord)
         ORDER BY u.ord",
    )
    .bind(&orgs)
    .bind(&ats)
    .bind(&actors)
    .bind(&principals)
    .bind(&actions)
    .bind(&resources)
    .bind(&allowed)
    .bind(&reasons)
    .bind(&requests)
    .execute(conn)
    .await?;
    Ok(done.rows_affected())
}

/// Entries matching `filter`, newest first (`at` descending, ties by id
/// descending).
pub async fn list_audit(
    conn: &mut PgConnection,
    filter: &AuditFilter,
) -> Result<Vec<AuditEntry>, StoreError> {
    if filter.limit == 0 || filter.limit > MAX_AUDIT_LISTED {
        return Err(StoreError::invalid(format!(
            "audit listing limit must be between 1 and {MAX_AUDIT_LISTED}"
        )));
    }
    let rows = sqlx::query_as::<_, AuditRow>(
        "SELECT id, organization_id, at, actor, principal_id, action, resource, allowed, reason,
                request_id, recorded_at
         FROM audit_log
         WHERE ($1::uuid IS NULL OR organization_id = $1)
           AND ($2::uuid IS NULL OR principal_id = $2)
           AND ($3::boolean IS NULL OR allowed = $3)
           AND ($4::text IS NULL OR action = $4)
           AND ($5::timestamptz IS NULL OR at >= $5)
           AND ($6::timestamptz IS NULL OR (at, id) < ($6, $7::uuid))
         ORDER BY at DESC, id DESC
         LIMIT $8",
    )
    .bind(filter.organization)
    .bind(filter.principal)
    .bind(filter.allowed)
    .bind(filter.action.as_deref())
    .bind(filter.since)
    .bind(filter.before.map(|c| c.at))
    .bind(filter.before.map(|c| c.id))
    .bind(i64::from(filter.limit))
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Retention: deletes entries with `at` before `before`, of one
/// organization or (with `None`) of all. Returns how many. This is the only
/// operation the table's append-only guard lets through.
pub async fn prune_audit_log(
    conn: &mut PgConnection,
    organization: Option<OrganizationId>,
    before: OffsetDateTime,
) -> Result<u64, StoreError> {
    let mut tx = conn.begin().await?;
    // `set_config(..., true)` is local to this transaction (or savepoint's
    // transaction), so the guard stays closed for everything else.
    sqlx::query("SELECT set_config('knowell.audit_prune', 'on', true)")
        .execute(&mut *tx)
        .await?;
    let done = sqlx::query(
        "DELETE FROM audit_log WHERE at < $1 AND ($2::uuid IS NULL OR organization_id = $2)",
    )
    .bind(before)
    .bind(organization)
    .execute(&mut *tx)
    .await?;
    sqlx::query("SELECT set_config('knowell.audit_prune', 'off', true)")
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(done.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry() -> NewAuditEntry {
        NewAuditEntry {
            organization: None,
            at: OffsetDateTime::UNIX_EPOCH,
            actor: "user:00000000-0000-0000-0000-000000000001".into(),
            principal: None,
            action: "read_uncommitted_overlay:00000000-0000-0000-0000-000000000002".into(),
            resource: "project:main/api".into(),
            allowed: false,
            reason: "denied_no_grant".into(),
            request_id: "0190d1c4-0000-7000-8000-000000000001".into(),
        }
    }

    #[test]
    fn entries_use_restricted_alphabets() {
        assert!(check_entry(&entry()).is_ok());
        let canary = "KNOWELL_CANARY secret value";
        for bad in [
            NewAuditEntry {
                actor: canary.into(),
                ..entry()
            },
            NewAuditEntry {
                resource: String::new(),
                ..entry()
            },
            NewAuditEntry {
                reason: "Denied".into(),
                ..entry()
            },
            NewAuditEntry {
                request_id: "a/b".into(),
                ..entry()
            },
            NewAuditEntry {
                action: "x".repeat(MAX_AUDIT_ACTION_BYTES + 1),
                ..entry()
            },
        ] {
            let err = check_entry(&bad).unwrap_err();
            assert!(!err.to_string().contains("CANARY"), "{err}");
        }
    }
}
