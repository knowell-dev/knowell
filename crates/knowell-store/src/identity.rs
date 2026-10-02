//! Identity persistence: principals, grants and API tokens.
//!
//! The domain (roles, authorization, token format and verification) lives in
//! `knowell-auth`; this module persists rows and the server maps them.
//! Agent sessions are not principals: an agent token belongs to the user it
//! acts for and names the agent client and session itself ([`TokenAgent`]).
//!
//! Tokens are stored without plaintext: the lookup prefix (`kn_` + 8
//! characters, not unique) and the 32-byte keyed hash. [`tokens_with_prefix`]
//! returns every candidate; the caller verifies the presented token in
//! constant time. A disabled principal's tokens report the disable time as
//! their effective revocation ([`StoredApiToken::effective_revoked_at`]) and
//! its grants are not returned by [`grants_for_principal`].

use std::time::Duration;

use knowell_core::Name;
use sqlx::PgConnection;
use time::OffsetDateTime;
use uuid::Uuid;

use crate::error::{StoreError, Violation, map_write, violation};
use crate::hierarchy::stored_name;
use crate::ids::{ApiTokenId, GrantId, OrganizationId, PrincipalId, ProjectId, WorkspaceId};
use crate::types::{ApiTokenScope, GrantRole, PrincipalKind, check_text};

/// Longest display name or token label, in bytes.
pub const MAX_DISPLAY_BYTES: usize = 200;

/// A principal to create.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewPrincipal {
    /// Id to use (e.g. a configured local user id); `None` = generated
    /// UUIDv7.
    pub id: Option<PrincipalId>,
    /// User or service account.
    pub kind: PrincipalKind,
    /// Login name, unique in the organization.
    pub name: Name,
    /// Name to show (1-200 bytes).
    pub display_name: Option<String>,
}

/// A stored principal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredPrincipal {
    /// Id (the knowell-auth `UserId` / `ServiceAccountId`).
    pub id: PrincipalId,
    /// Owning organization.
    pub organization: OrganizationId,
    /// User or service account.
    pub kind: PrincipalKind,
    /// Login name.
    pub name: Name,
    /// Name to show.
    pub display_name: Option<String>,
    /// When it was disabled, if it is.
    pub disabled_at: Option<OffsetDateTime>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// Where a grant applies (store form of `knowell_auth::ResourceScope`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum GrantScope {
    /// The whole organization.
    Organization,
    /// One workspace and its projects.
    Workspace(WorkspaceId),
    /// One project (its workspace is looked up).
    Project(ProjectId),
}

/// A grant to create.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct NewGrant {
    /// The grantee.
    pub principal: PrincipalId,
    /// The role.
    pub role: GrantRole,
    /// Where it applies (inside the principal's organization).
    pub scope: GrantScope,
    /// Who granted it.
    pub created_by: Option<PrincipalId>,
}

/// A stored grant with the current names of its scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredGrant {
    /// Id.
    pub id: GrantId,
    /// Organization.
    pub organization: OrganizationId,
    /// The grantee.
    pub principal: PrincipalId,
    /// The grantee's kind.
    pub principal_kind: PrincipalKind,
    /// The role.
    pub role: GrantRole,
    /// Where it applies.
    pub scope: GrantScope,
    /// Name of the scope's workspace (workspace and project grants).
    pub workspace_name: Option<Name>,
    /// Name of the scope's project (project grants).
    pub project_name: Option<Name>,
    /// Who granted it (`None` when unknown or since deleted).
    pub created_by: Option<PrincipalId>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// The agent an agent token acts as.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TokenAgent {
    /// The agent client, e.g. `claude-code`.
    pub client: Name,
    /// The agent session.
    pub session: Uuid,
}

/// An API token to store. Holds no plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewApiToken {
    /// Id (the knowell-auth `TokenId`).
    pub id: ApiTokenId,
    /// The principal the token authenticates as (for an agent token: the
    /// user the agent acts for).
    pub principal: PrincipalId,
    /// Set for agent tokens (user principals only; must expire within 24
    /// hours and cannot carry the admin scope).
    pub agent: Option<TokenAgent>,
    /// Lookup prefix: `kn_` followed by 8 characters of `[a-z2-7]`.
    pub prefix: String,
    /// Keyed hash of the token payload (32 bytes).
    pub key_hash: [u8; 32],
    /// What the token may do at most (at least one).
    pub scopes: Vec<ApiTokenScope>,
    /// Name to show (1-200 bytes).
    pub label: Option<String>,
    /// Who issued it.
    pub created_by: Option<PrincipalId>,
    /// Issue time.
    pub created_at: OffsetDateTime,
    /// Expiry (after `created_at`).
    pub expires_at: Option<OffsetDateTime>,
}

/// A stored API token. Holds no plaintext.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredApiToken {
    /// Id.
    pub id: ApiTokenId,
    /// Organization.
    pub organization: OrganizationId,
    /// The principal it authenticates as.
    pub principal: PrincipalId,
    /// That principal's kind.
    pub principal_kind: PrincipalKind,
    /// When that principal was disabled, if it is.
    pub principal_disabled_at: Option<OffsetDateTime>,
    /// Set for agent tokens.
    pub agent: Option<TokenAgent>,
    /// Lookup prefix.
    pub prefix: String,
    /// Keyed hash.
    pub key_hash: [u8; 32],
    /// Scopes, in ascending order.
    pub scopes: Vec<ApiTokenScope>,
    /// Name to show.
    pub label: Option<String>,
    /// Who issued it.
    pub created_by: Option<PrincipalId>,
    /// Issue time.
    pub created_at: OffsetDateTime,
    /// Expiry.
    pub expires_at: Option<OffsetDateTime>,
    /// Revocation time.
    pub revoked_at: Option<OffsetDateTime>,
    /// Last successful use, updated at most once per throttle interval.
    pub last_used_at: Option<OffsetDateTime>,
}

impl StoredApiToken {
    /// The earlier of the token's revocation and its principal's disable
    /// time: from then on the token must not authenticate.
    pub fn effective_revoked_at(&self) -> Option<OffsetDateTime> {
        match (self.revoked_at, self.principal_disabled_at) {
            (Some(a), Some(b)) => Some(a.min(b)),
            (a, b) => a.or(b),
        }
    }
}

#[derive(sqlx::FromRow)]
struct PrincipalRow {
    id: PrincipalId,
    organization_id: OrganizationId,
    kind: PrincipalKind,
    name: String,
    display_name: Option<String>,
    disabled_at: Option<OffsetDateTime>,
    created_at: OffsetDateTime,
}

impl TryFrom<PrincipalRow> for StoredPrincipal {
    type Error = StoreError;

    fn try_from(row: PrincipalRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            organization: row.organization_id,
            kind: row.kind,
            name: stored_name(row.name)?,
            display_name: row.display_name,
            disabled_at: row.disabled_at,
            created_at: row.created_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct GrantRow {
    id: GrantId,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    principal_kind: PrincipalKind,
    role: GrantRole,
    level: String,
    workspace_id: Option<WorkspaceId>,
    project_id: Option<ProjectId>,
    workspace_name: Option<String>,
    project_name: Option<String>,
    created_by: Option<PrincipalId>,
    created_at: OffsetDateTime,
}

impl TryFrom<GrantRow> for StoredGrant {
    type Error = StoreError;

    fn try_from(row: GrantRow) -> Result<Self, StoreError> {
        let corrupt = || StoreError::Corrupt(format!("grant {} has inconsistent scope", row.id));
        let scope = match row.level.as_str() {
            "organization" => GrantScope::Organization,
            "workspace" => GrantScope::Workspace(row.workspace_id.ok_or_else(corrupt)?),
            "project" => GrantScope::Project(row.project_id.ok_or_else(corrupt)?),
            _ => return Err(corrupt()),
        };
        Ok(Self {
            id: row.id,
            organization: row.organization_id,
            principal: row.principal_id,
            principal_kind: row.principal_kind,
            role: row.role,
            scope,
            workspace_name: row.workspace_name.map(stored_name).transpose()?,
            project_name: row.project_name.map(stored_name).transpose()?,
            created_by: row.created_by,
            created_at: row.created_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct TokenRow {
    id: ApiTokenId,
    organization_id: OrganizationId,
    principal_id: PrincipalId,
    principal_kind: PrincipalKind,
    principal_disabled_at: Option<OffsetDateTime>,
    agent_client: Option<String>,
    agent_session: Option<Uuid>,
    prefix: String,
    key_hash: Vec<u8>,
    scopes: Vec<ApiTokenScope>,
    label: Option<String>,
    created_by: Option<PrincipalId>,
    created_at: OffsetDateTime,
    expires_at: Option<OffsetDateTime>,
    revoked_at: Option<OffsetDateTime>,
    last_used_at: Option<OffsetDateTime>,
}

impl TryFrom<TokenRow> for StoredApiToken {
    type Error = StoreError;

    fn try_from(row: TokenRow) -> Result<Self, StoreError> {
        let key_hash: [u8; 32] = row.key_hash.as_slice().try_into().map_err(|_| {
            StoreError::Corrupt(format!("api token {} has a malformed hash", row.id))
        })?;
        let agent = match (row.agent_client, row.agent_session) {
            (Some(client), Some(session)) => Some(TokenAgent {
                client: stored_name(client)?,
                session,
            }),
            (None, None) => None,
            _ => {
                return Err(StoreError::Corrupt(format!(
                    "api token {} has half of an agent",
                    row.id
                )));
            }
        };
        let mut scopes = row.scopes;
        scopes.sort();
        scopes.dedup();
        Ok(Self {
            id: row.id,
            organization: row.organization_id,
            principal: row.principal_id,
            principal_kind: row.principal_kind,
            principal_disabled_at: row.principal_disabled_at,
            agent,
            prefix: row.prefix,
            key_hash,
            scopes,
            label: row.label,
            created_by: row.created_by,
            created_at: row.created_at,
            expires_at: row.expires_at,
            revoked_at: row.revoked_at,
            last_used_at: row.last_used_at,
        })
    }
}

macro_rules! principal_columns {
    () => {
        "id, organization_id, kind, name, display_name, disabled_at, created_at"
    };
}

macro_rules! grant_select {
    () => {
        "SELECT g.id, g.organization_id, g.principal_id, p.kind AS principal_kind, g.role,
                g.level::text AS level, g.workspace_id, g.project_id,
                w.name AS workspace_name, pr.name AS project_name, g.created_by, g.created_at
         FROM access_grant g
         JOIN principal p ON p.id = g.principal_id
         LEFT JOIN workspace w ON w.id = g.workspace_id
         LEFT JOIN project pr ON pr.id = g.project_id"
    };
}

macro_rules! token_select {
    () => {
        "SELECT t.id, t.organization_id, t.principal_id, p.kind AS principal_kind,
                p.disabled_at AS principal_disabled_at, t.agent_client, t.agent_session,
                t.prefix, t.key_hash, t.scopes, t.label, t.created_by, t.created_at,
                t.expires_at, t.revoked_at, t.last_used_at
         FROM api_token t JOIN principal p ON p.id = t.principal_id"
    };
}

/// Creates a principal. Fails with [`StoreError::AlreadyExists`] when the
/// name or id is taken and [`StoreError::NotFound`] for an unknown
/// organization.
pub async fn create_principal(
    conn: &mut PgConnection,
    organization: OrganizationId,
    principal: &NewPrincipal,
) -> Result<StoredPrincipal, StoreError> {
    if let Some(display) = &principal.display_name {
        check_text("display name", display, MAX_DISPLAY_BYTES, false)?;
    }
    sqlx::query_as::<_, PrincipalRow>(concat!(
        "INSERT INTO principal (id, organization_id, kind, name, display_name)
         VALUES (coalesce($1, knowell_uuidv7()), $2, $3, $4, $5)
         RETURNING ",
        principal_columns!()
    ))
    .bind(principal.id)
    .bind(organization)
    .bind(principal.kind)
    .bind(principal.name.as_str())
    .bind(principal.display_name.as_deref())
    .fetch_one(conn)
    .await
    .map_err(|e| {
        map_write(
            e,
            "principal",
            &principal.name,
            "organization",
            organization,
        )
    })?
    .try_into()
}

/// Looks a principal up by id.
pub async fn get_principal(
    conn: &mut PgConnection,
    id: PrincipalId,
) -> Result<Option<StoredPrincipal>, StoreError> {
    let row = sqlx::query_as::<_, PrincipalRow>(concat!(
        "SELECT ",
        principal_columns!(),
        " FROM principal WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Looks a principal up by name within an organization.
pub async fn find_principal(
    conn: &mut PgConnection,
    organization: OrganizationId,
    name: &Name,
) -> Result<Option<StoredPrincipal>, StoreError> {
    let row = sqlx::query_as::<_, PrincipalRow>(concat!(
        "SELECT ",
        principal_columns!(),
        " FROM principal WHERE organization_id = $1 AND name = $2"
    ))
    .bind(organization)
    .bind(name.as_str())
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// An organization's principals by name.
pub async fn list_principals(
    conn: &mut PgConnection,
    organization: OrganizationId,
) -> Result<Vec<StoredPrincipal>, StoreError> {
    sqlx::query_as::<_, PrincipalRow>(concat!(
        "SELECT ",
        principal_columns!(),
        " FROM principal WHERE organization_id = $1 ORDER BY name COLLATE \"C\""
    ))
    .bind(organization)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Disables a principal at `at` (`Some`) or enables it again (`None`).
/// Disabling an already disabled principal keeps the earlier time.
pub async fn set_principal_disabled(
    conn: &mut PgConnection,
    id: PrincipalId,
    at: Option<OffsetDateTime>,
) -> Result<StoredPrincipal, StoreError> {
    let row = sqlx::query_as::<_, PrincipalRow>(concat!(
        "UPDATE principal
         SET disabled_at = CASE WHEN $2::timestamptz IS NULL THEN NULL
                                ELSE least(disabled_at, $2) END
         WHERE id = $1
         RETURNING ",
        principal_columns!()
    ))
    .bind(id)
    .bind(at)
    .fetch_optional(conn)
    .await?;
    row.ok_or_else(|| StoreError::not_found("principal", id))?
        .try_into()
}

/// Deletes a principal with its grants and tokens. Returns whether it
/// existed. Audit entries keep its id.
pub async fn delete_principal(
    conn: &mut PgConnection,
    id: PrincipalId,
) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM principal WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Grants a role. The scope must be in the principal's organization (a
/// foreign scope is reported as not found). Fails with
/// [`StoreError::AlreadyExists`] for an identical grant.
pub async fn create_grant(
    conn: &mut PgConnection,
    grant: &NewGrant,
) -> Result<StoredGrant, StoreError> {
    let org: Option<OrganizationId> =
        sqlx::query_scalar("SELECT organization_id FROM principal WHERE id = $1")
            .bind(grant.principal)
            .fetch_optional(&mut *conn)
            .await?;
    let org = org.ok_or_else(|| StoreError::not_found("principal", grant.principal))?;
    let (level, workspace, project) = match grant.scope {
        GrantScope::Organization => ("organization", None, None),
        GrantScope::Workspace(ws) => ("workspace", Some(ws), None),
        GrantScope::Project(p) => {
            let row: Option<(WorkspaceId, OrganizationId)> =
                sqlx::query_as("SELECT workspace_id, organization_id FROM project WHERE id = $1")
                    .bind(p)
                    .fetch_optional(&mut *conn)
                    .await?;
            match row {
                Some((ws, project_org)) if project_org == org => ("project", Some(ws), Some(p)),
                _ => return Err(StoreError::not_found("project", p)),
            }
        }
    };
    let id: GrantId = sqlx::query_scalar(
        "INSERT INTO access_grant (organization_id, principal_id, role, level, workspace_id,
                                   project_id, created_by)
         VALUES ($1, $2, $3, $4::grant_level, $5, $6, $7)
         RETURNING id",
    )
    .bind(org)
    .bind(grant.principal)
    .bind(grant.role)
    .bind(level)
    .bind(workspace)
    .bind(project)
    .bind(grant.created_by)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::Unique(_)) => StoreError::already_exists("grant", grant.principal),
        Some(Violation::ForeignKey(c)) => match c.as_deref() {
            Some("access_grant_workspace_fk") => StoreError::not_found(
                "workspace in this organization",
                workspace.map(|w| w.to_string()).unwrap_or_default(),
            ),
            Some("access_grant_created_by_fkey") => StoreError::not_found(
                "principal",
                grant.created_by.map(|p| p.to_string()).unwrap_or_default(),
            ),
            _ => StoreError::not_found("principal", grant.principal),
        },
        _ => StoreError::Database(e),
    })?;
    let row = sqlx::query_as::<_, GrantRow>(concat!(grant_select!(), " WHERE g.id = $1"))
        .bind(id)
        .fetch_one(conn)
        .await?;
    row.try_into()
}

/// An organization's grants, optionally of one principal, ordered by
/// principal, then level (organization first), role and id.
pub async fn list_grants(
    conn: &mut PgConnection,
    organization: OrganizationId,
    principal: Option<PrincipalId>,
) -> Result<Vec<StoredGrant>, StoreError> {
    sqlx::query_as::<_, GrantRow>(concat!(
        grant_select!(),
        " WHERE g.organization_id = $1 AND ($2::uuid IS NULL OR g.principal_id = $2)
         ORDER BY g.principal_id, g.level, g.role, g.id"
    ))
    .bind(organization)
    .bind(principal)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// The grants a request by `principal` is evaluated against: none when the
/// principal is disabled or unknown. Ordered like [`list_grants`].
pub async fn grants_for_principal(
    conn: &mut PgConnection,
    principal: PrincipalId,
) -> Result<Vec<StoredGrant>, StoreError> {
    sqlx::query_as::<_, GrantRow>(concat!(
        grant_select!(),
        " WHERE g.principal_id = $1 AND p.disabled_at IS NULL
         ORDER BY g.principal_id, g.level, g.role, g.id"
    ))
    .bind(principal)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Removes a grant. Returns whether it existed.
pub async fn delete_grant(conn: &mut PgConnection, id: GrantId) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM access_grant WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Checks a lookup prefix: `kn_` followed by 8 characters of `[a-z2-7]`.
fn check_prefix(prefix: &str) -> Result<(), StoreError> {
    let ok = prefix.len() == 11
        && prefix.starts_with("kn_")
        && prefix
            .bytes()
            .skip(3)
            .all(|b| b.is_ascii_lowercase() || (b'2'..=b'7').contains(&b));
    if ok {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "token prefix must be `kn_` followed by 8 characters of [a-z2-7]",
        ))
    }
}

/// Stores an API token. Fails with [`StoreError::NotFound`] for an unknown
/// principal, [`StoreError::AlreadyExists`] for a taken id or hash, and
/// [`StoreError::InvalidInput`] for an agent token of a service account or
/// one breaking the agent limits (24 hours, no admin scope).
pub async fn insert_api_token(
    conn: &mut PgConnection,
    token: &NewApiToken,
) -> Result<StoredApiToken, StoreError> {
    check_prefix(&token.prefix)?;
    if token.scopes.is_empty() {
        return Err(StoreError::invalid("a token needs at least one scope"));
    }
    if let Some(label) = &token.label {
        check_text("token label", label, MAX_DISPLAY_BYTES, false)?;
    }
    if token.expires_at.is_some_and(|e| e <= token.created_at) {
        return Err(StoreError::invalid(
            "token expiry must be after its issue time",
        ));
    }
    let principal: Option<(OrganizationId, PrincipalKind)> =
        sqlx::query_as("SELECT organization_id, kind FROM principal WHERE id = $1")
            .bind(token.principal)
            .fetch_optional(&mut *conn)
            .await?;
    let (org, kind) =
        principal.ok_or_else(|| StoreError::not_found("principal", token.principal))?;
    if token.agent.is_some() && kind != PrincipalKind::User {
        return Err(StoreError::invalid(
            "agent tokens act for a user; the principal is a service account",
        ));
    }
    sqlx::query(
        "INSERT INTO api_token (id, organization_id, principal_id, agent_client, agent_session,
                                prefix, key_hash, scopes, label, created_by, created_at,
                                expires_at)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8, $9, $10, $11, $12)",
    )
    .bind(token.id)
    .bind(org)
    .bind(token.principal)
    .bind(token.agent.as_ref().map(|a| a.client.as_str()))
    .bind(token.agent.as_ref().map(|a| a.session))
    .bind(&token.prefix)
    .bind(token.key_hash.as_slice())
    .bind(&token.scopes)
    .bind(token.label.as_deref())
    .bind(token.created_by)
    .bind(token.created_at)
    .bind(token.expires_at)
    .execute(&mut *conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::Unique(_)) => StoreError::already_exists("api token", token.id),
        Some(Violation::ForeignKey(c)) if c.as_deref() == Some("api_token_created_by_fkey") => {
            StoreError::not_found(
                "principal",
                token.created_by.map(|p| p.to_string()).unwrap_or_default(),
            )
        }
        Some(Violation::ForeignKey(_)) => StoreError::not_found("principal", token.principal),
        Some(Violation::Check(c)) => {
            StoreError::invalid(format!("api token violates `{}`", c.unwrap_or_default()))
        }
        None => StoreError::Database(e),
    })?;
    get_api_token(conn, token.id)
        .await?
        .ok_or_else(|| StoreError::Corrupt("inserted api token vanished".to_owned()))
}

/// Looks a token up by id.
pub async fn get_api_token(
    conn: &mut PgConnection,
    id: ApiTokenId,
) -> Result<Option<StoredApiToken>, StoreError> {
    let row = sqlx::query_as::<_, TokenRow>(concat!(token_select!(), " WHERE t.id = $1"))
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(TryInto::try_into).transpose()
}

/// Every token of `organization` with lookup prefix `prefix`, including
/// revoked, expired and disabled principals' ones (the caller verifies and
/// reports those states). Ordered by id. A malformed prefix matches nothing.
pub async fn tokens_with_prefix(
    conn: &mut PgConnection,
    organization: OrganizationId,
    prefix: &str,
) -> Result<Vec<StoredApiToken>, StoreError> {
    if check_prefix(prefix).is_err() {
        return Ok(Vec::new());
    }
    sqlx::query_as::<_, TokenRow>(concat!(
        token_select!(),
        " WHERE t.prefix = $1 AND t.organization_id = $2 ORDER BY t.id"
    ))
    .bind(prefix)
    .bind(organization)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// An organization's tokens, optionally of one principal, newest first.
pub async fn list_api_tokens(
    conn: &mut PgConnection,
    organization: OrganizationId,
    principal: Option<PrincipalId>,
) -> Result<Vec<StoredApiToken>, StoreError> {
    sqlx::query_as::<_, TokenRow>(concat!(
        token_select!(),
        " WHERE t.organization_id = $1 AND ($2::uuid IS NULL OR t.principal_id = $2)
         ORDER BY t.created_at DESC, t.id DESC"
    ))
    .bind(organization)
    .bind(principal)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Revokes a token at `at`. Idempotent: an earlier revocation time is kept.
/// Returns whether the token exists.
pub async fn revoke_api_token(
    conn: &mut PgConnection,
    id: ApiTokenId,
    at: OffsetDateTime,
) -> Result<bool, StoreError> {
    let done = sqlx::query("UPDATE api_token SET revoked_at = least(revoked_at, $2) WHERE id = $1")
        .bind(id)
        .bind(at)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Records a successful use at `at`, unless the stored `last_used_at` is
/// less than `min_interval` older (so busy tokens cause at most one write
/// per interval, also across server replicas). Returns whether it wrote.
pub async fn touch_api_token(
    conn: &mut PgConnection,
    id: ApiTokenId,
    at: OffsetDateTime,
    min_interval: Duration,
) -> Result<bool, StoreError> {
    let interval_ms = i64::try_from(min_interval.as_millis())
        .map_err(|_| StoreError::invalid("touch interval is too long"))?;
    let done = sqlx::query(
        "UPDATE api_token SET last_used_at = $2
         WHERE id = $1
           AND (last_used_at IS NULL
                OR last_used_at <= $2 - $3 * interval '1 millisecond')",
    )
    .bind(id)
    .bind(at)
    .bind(interval_ms)
    .execute(conn)
    .await?;
    Ok(done.rows_affected() > 0)
}

/// Deletes a token record. Returns whether it existed.
pub async fn delete_api_token(conn: &mut PgConnection, id: ApiTokenId) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM api_token WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn prefixes_are_checked() {
        assert!(check_prefix("kn_abcdefgh").is_ok());
        assert!(check_prefix("kn_a2b3c4d7").is_ok());
        for bad in [
            "",
            "kn_",
            "kn_abcdefg",
            "kn_abcdefghi",
            "kn_ABCDEFGH",
            "kn_abcdefg1",
            "xx_abcdefgh",
        ] {
            assert!(check_prefix(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn effective_revocation_is_the_earlier_time() {
        let t = |s: i64| OffsetDateTime::from_unix_timestamp(s).unwrap();
        let mut token = StoredApiToken {
            id: ApiTokenId(Uuid::nil()),
            organization: OrganizationId(Uuid::nil()),
            principal: PrincipalId(Uuid::nil()),
            principal_kind: PrincipalKind::User,
            principal_disabled_at: None,
            agent: None,
            prefix: "kn_abcdefgh".into(),
            key_hash: [0; 32],
            scopes: vec![ApiTokenScope::Read],
            label: None,
            created_by: None,
            created_at: t(0),
            expires_at: None,
            revoked_at: None,
            last_used_at: None,
        };
        assert_eq!(token.effective_revoked_at(), None);
        token.revoked_at = Some(t(20));
        assert_eq!(token.effective_revoked_at(), Some(t(20)));
        token.principal_disabled_at = Some(t(10));
        assert_eq!(token.effective_revoked_at(), Some(t(10)));
        token.revoked_at = None;
        assert_eq!(token.effective_revoked_at(), Some(t(10)));
    }
}
