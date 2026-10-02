//! Organization -> workspace -> project, and the sources projects live in.
//!
//! Names are unique per parent; creating a duplicate returns
//! [`StoreError::AlreadyExists`].

use knowell_core::{Name, RepoPath};
use sqlx::PgConnection;
use time::OffsetDateTime;

use crate::error::{StoreError, map_write};
use crate::ids::{OrganizationId, ProjectId, SourceId, WorkspaceId};
use crate::types::SourceKind;

/// A tenant: the security boundary nothing is shared across.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Organization {
    /// Id.
    pub id: OrganizationId,
    /// Unique name.
    pub name: Name,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// A set of projects with shared settings (ref policy, profile, memory).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Workspace {
    /// Id.
    pub id: WorkspaceId,
    /// Owning organization.
    pub organization: OrganizationId,
    /// Name, unique within the organization.
    pub name: Name,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// A git repository or a plain directory.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Source {
    /// Id.
    pub id: SourceId,
    /// Owning organization.
    pub organization: OrganizationId,
    /// Git repository or directory.
    pub kind: SourceKind,
    /// Where it lives (path or clone URL), unique within the organization.
    /// Never contains credentials.
    pub location: String,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

/// A project: a root inside a source, belonging to one workspace.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Project {
    /// Id.
    pub id: ProjectId,
    /// Owning organization (same as the workspace's and the source's).
    pub organization: OrganizationId,
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Source the project's files come from.
    pub source: SourceId,
    /// Name, unique within the workspace.
    pub name: Name,
    /// Root inside the source; `None` for the source root.
    pub root: Option<RepoPath>,
    /// Creation time.
    pub created_at: OffsetDateTime,
}

#[derive(sqlx::FromRow)]
struct OrganizationRow {
    id: OrganizationId,
    name: String,
    created_at: OffsetDateTime,
}

impl TryFrom<OrganizationRow> for Organization {
    type Error = StoreError;

    fn try_from(row: OrganizationRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            name: stored_name(row.name)?,
            created_at: row.created_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct WorkspaceRow {
    id: WorkspaceId,
    organization_id: OrganizationId,
    name: String,
    created_at: OffsetDateTime,
}

impl TryFrom<WorkspaceRow> for Workspace {
    type Error = StoreError;

    fn try_from(row: WorkspaceRow) -> Result<Self, StoreError> {
        Ok(Self {
            id: row.id,
            organization: row.organization_id,
            name: stored_name(row.name)?,
            created_at: row.created_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct SourceRow {
    id: SourceId,
    organization_id: OrganizationId,
    kind: SourceKind,
    location: String,
    created_at: OffsetDateTime,
}

impl From<SourceRow> for Source {
    fn from(row: SourceRow) -> Self {
        Self {
            id: row.id,
            organization: row.organization_id,
            kind: row.kind,
            location: row.location,
            created_at: row.created_at,
        }
    }
}

#[derive(sqlx::FromRow)]
struct ProjectRow {
    id: ProjectId,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    source_id: SourceId,
    name: String,
    root_path: String,
    created_at: OffsetDateTime,
}

impl TryFrom<ProjectRow> for Project {
    type Error = StoreError;

    fn try_from(row: ProjectRow) -> Result<Self, StoreError> {
        let root = if row.root_path.is_empty() {
            None
        } else {
            Some(stored_path(row.root_path)?)
        };
        Ok(Self {
            id: row.id,
            organization: row.organization_id,
            workspace: row.workspace_id,
            source: row.source_id,
            name: stored_name(row.name)?,
            root,
            created_at: row.created_at,
        })
    }
}

pub(crate) fn stored_name(text: String) -> Result<Name, StoreError> {
    Name::new(text).map_err(|e| StoreError::Corrupt(format!("stored name: {e}")))
}

pub(crate) fn stored_path(text: String) -> Result<RepoPath, StoreError> {
    RepoPath::new(text).map_err(|e| StoreError::Corrupt(format!("stored path: {e}")))
}

macro_rules! workspace_columns {
    () => {
        "id, organization_id, name, created_at"
    };
}
macro_rules! source_columns {
    () => {
        "id, organization_id, kind, location, created_at"
    };
}
macro_rules! project_columns {
    () => {
        "id, organization_id, workspace_id, source_id, name, root_path, created_at"
    };
}

/// Creates an organization.
pub async fn create_organization(
    conn: &mut PgConnection,
    name: &Name,
) -> Result<Organization, StoreError> {
    sqlx::query_as::<_, OrganizationRow>(
        "INSERT INTO organization (name) VALUES ($1) RETURNING id, name, created_at",
    )
    .bind(name.as_str())
    .fetch_one(conn)
    .await
    .map_err(|e| map_write(e, "organization", name, "organization", name))?
    .try_into()
}

/// Looks an organization up by name.
pub async fn find_organization(
    conn: &mut PgConnection,
    name: &Name,
) -> Result<Option<Organization>, StoreError> {
    let row = sqlx::query_as::<_, OrganizationRow>(
        "SELECT id, name, created_at FROM organization WHERE name = $1",
    )
    .bind(name.as_str())
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Lists organizations by name.
pub async fn list_organizations(conn: &mut PgConnection) -> Result<Vec<Organization>, StoreError> {
    sqlx::query_as::<_, OrganizationRow>(
        "SELECT id, name, created_at FROM organization ORDER BY name COLLATE \"C\"",
    )
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Deletes an organization and everything in it. Returns whether it existed.
pub async fn delete_organization(
    conn: &mut PgConnection,
    id: OrganizationId,
) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM organization WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Creates a workspace in an organization.
pub async fn create_workspace(
    conn: &mut PgConnection,
    organization: OrganizationId,
    name: &Name,
) -> Result<Workspace, StoreError> {
    sqlx::query_as::<_, WorkspaceRow>(
        "INSERT INTO workspace (organization_id, name) VALUES ($1, $2)
         RETURNING id, organization_id, name, created_at",
    )
    .bind(organization)
    .bind(name.as_str())
    .fetch_one(conn)
    .await
    .map_err(|e| map_write(e, "workspace", name, "organization", organization))?
    .try_into()
}

/// Looks a workspace up by id.
pub async fn get_workspace(
    conn: &mut PgConnection,
    id: WorkspaceId,
) -> Result<Option<Workspace>, StoreError> {
    let sql = concat!(
        "SELECT ",
        workspace_columns!(),
        " FROM workspace WHERE id = $1"
    );
    let row = sqlx::query_as::<_, WorkspaceRow>(sql)
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(TryInto::try_into).transpose()
}

/// Looks a workspace up by name within an organization.
pub async fn find_workspace(
    conn: &mut PgConnection,
    organization: OrganizationId,
    name: &Name,
) -> Result<Option<Workspace>, StoreError> {
    let row = sqlx::query_as::<_, WorkspaceRow>(
        "SELECT id, organization_id, name, created_at FROM workspace
         WHERE organization_id = $1 AND name = $2",
    )
    .bind(organization)
    .bind(name.as_str())
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Lists an organization's workspaces by name.
pub async fn list_workspaces(
    conn: &mut PgConnection,
    organization: OrganizationId,
) -> Result<Vec<Workspace>, StoreError> {
    sqlx::query_as::<_, WorkspaceRow>(
        "SELECT id, organization_id, name, created_at FROM workspace
         WHERE organization_id = $1 ORDER BY name COLLATE \"C\"",
    )
    .bind(organization)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Renames a workspace. Returns [`StoreError::AlreadyExists`] if the new name
/// is taken in the organization.
pub async fn rename_workspace(
    conn: &mut PgConnection,
    id: WorkspaceId,
    name: &Name,
) -> Result<Workspace, StoreError> {
    let row = sqlx::query_as::<_, WorkspaceRow>(
        "UPDATE workspace SET name = $2 WHERE id = $1
         RETURNING id, organization_id, name, created_at",
    )
    .bind(id)
    .bind(name.as_str())
    .fetch_optional(conn)
    .await
    .map_err(|e| map_write(e, "workspace", name, "workspace", id))?;
    row.ok_or_else(|| StoreError::not_found("workspace", id))?
        .try_into()
}

/// Deletes a workspace and its projects. Returns whether it existed.
pub async fn delete_workspace(
    conn: &mut PgConnection,
    id: WorkspaceId,
) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM workspace WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Registers a source. `location` is a local path or a clone URL; URLs that
/// embed credentials are rejected (use a credential helper or a secret
/// reference instead), and the rejected value is never echoed.
pub async fn create_source(
    conn: &mut PgConnection,
    organization: OrganizationId,
    kind: SourceKind,
    location: &str,
) -> Result<Source, StoreError> {
    validate_location(location)?;
    let sql = concat!(
        "INSERT INTO source (organization_id, kind, location) VALUES ($1, $2, $3)
         RETURNING ",
        source_columns!()
    );
    let row = sqlx::query_as::<_, SourceRow>(sql)
        .bind(organization)
        .bind(kind)
        .bind(location)
        .fetch_one(conn)
        .await
        .map_err(|e| map_write(e, "source", location, "organization", organization))?;
    Ok(row.into())
}

/// Looks a source up by id.
pub async fn get_source(
    conn: &mut PgConnection,
    id: SourceId,
) -> Result<Option<Source>, StoreError> {
    let sql = concat!("SELECT ", source_columns!(), " FROM source WHERE id = $1");
    let row = sqlx::query_as::<_, SourceRow>(sql)
        .bind(id)
        .fetch_optional(conn)
        .await?;
    Ok(row.map(Into::into))
}

/// Looks a source up by location within an organization.
pub async fn find_source(
    conn: &mut PgConnection,
    organization: OrganizationId,
    location: &str,
) -> Result<Option<Source>, StoreError> {
    let sql = concat!(
        "SELECT ",
        source_columns!(),
        " FROM source WHERE organization_id = $1 AND location = $2"
    );
    let row = sqlx::query_as::<_, SourceRow>(sql)
        .bind(organization)
        .bind(location)
        .fetch_optional(conn)
        .await?;
    Ok(row.map(Into::into))
}

/// Lists an organization's sources by location.
pub async fn list_sources(
    conn: &mut PgConnection,
    organization: OrganizationId,
) -> Result<Vec<Source>, StoreError> {
    let sql = concat!(
        "SELECT ",
        source_columns!(),
        " FROM source WHERE organization_id = $1 ORDER BY location COLLATE \"C\""
    );
    let rows = sqlx::query_as::<_, SourceRow>(sql)
        .bind(organization)
        .fetch_all(conn)
        .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Deletes a source. Fails with [`StoreError::InvalidInput`] while projects
/// still use it. Returns whether it existed.
pub async fn delete_source(conn: &mut PgConnection, id: SourceId) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM source WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await
        .map_err(|e| match crate::error::violation(&e) {
            Some(crate::error::Violation::ForeignKey(_)) => {
                StoreError::invalid(format!("source {id} is still used by projects"))
            }
            _ => StoreError::Database(e),
        })?;
    Ok(done.rows_affected() > 0)
}

/// Creates a project in a workspace, rooted at `root` inside `source`
/// (`None` = the source root). Workspace and source must belong to the same
/// organization.
pub async fn create_project(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    source: SourceId,
    name: &Name,
    root: Option<&RepoPath>,
) -> Result<Project, StoreError> {
    let root_text = root.map(RepoPath::as_str).unwrap_or_default();
    // The organization comes from the workspace; the composite foreign key
    // then rejects a source of another organization.
    let sql = concat!(
        "INSERT INTO project (organization_id, workspace_id, source_id, name, root_path)
         SELECT w.organization_id, w.id, $2, $3, $4 FROM workspace w WHERE w.id = $1
         RETURNING ",
        project_columns!()
    );
    let row = sqlx::query_as::<_, ProjectRow>(sql)
        .bind(workspace)
        .bind(source)
        .bind(name.as_str())
        .bind(root_text)
        .fetch_optional(conn)
        .await
        .map_err(|e| map_write(e, "project", name, "source in this organization", source))?;
    row.ok_or_else(|| StoreError::not_found("workspace", workspace))?
        .try_into()
}

/// Looks a project up by id.
pub async fn get_project(
    conn: &mut PgConnection,
    id: ProjectId,
) -> Result<Option<Project>, StoreError> {
    let sql = concat!("SELECT ", project_columns!(), " FROM project WHERE id = $1");
    let row = sqlx::query_as::<_, ProjectRow>(sql)
        .bind(id)
        .fetch_optional(conn)
        .await?;
    row.map(TryInto::try_into).transpose()
}

/// Looks a project up by name within a workspace.
pub async fn find_project(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    name: &Name,
) -> Result<Option<Project>, StoreError> {
    let sql = concat!(
        "SELECT ",
        project_columns!(),
        " FROM project WHERE workspace_id = $1 AND name = $2"
    );
    let row = sqlx::query_as::<_, ProjectRow>(sql)
        .bind(workspace)
        .bind(name.as_str())
        .fetch_optional(conn)
        .await?;
    row.map(TryInto::try_into).transpose()
}

/// Lists a workspace's projects by name.
pub async fn list_projects(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
) -> Result<Vec<Project>, StoreError> {
    let sql = concat!(
        "SELECT ",
        project_columns!(),
        " FROM project WHERE workspace_id = $1 ORDER BY name COLLATE \"C\""
    );
    sqlx::query_as::<_, ProjectRow>(sql)
        .bind(workspace)
        .fetch_all(conn)
        .await?
        .into_iter()
        .map(TryInto::try_into)
        .collect()
}

/// Renames a project within its workspace.
pub async fn rename_project(
    conn: &mut PgConnection,
    id: ProjectId,
    name: &Name,
) -> Result<Project, StoreError> {
    let sql = concat!(
        "UPDATE project SET name = $2 WHERE id = $1 RETURNING ",
        project_columns!()
    );
    let row = sqlx::query_as::<_, ProjectRow>(sql)
        .bind(id)
        .bind(name.as_str())
        .fetch_optional(conn)
        .await
        .map_err(|e| map_write(e, "project", name, "project", id))?;
    row.ok_or_else(|| StoreError::not_found("project", id))?
        .try_into()
}

/// Deletes a project and its views. Returns whether it existed.
pub async fn delete_project(conn: &mut PgConnection, id: ProjectId) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM project WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Rejects empty, oversized or credential-carrying locations.
fn validate_location(location: &str) -> Result<(), StoreError> {
    const MAX_LEN: usize = 4096;
    if location.is_empty() || location.len() > MAX_LEN || location.contains('\0') {
        return Err(StoreError::invalid(
            "source location must be 1-4096 bytes without NUL characters",
        ));
    }
    if let Some((scheme, rest)) = location.split_once("://") {
        let authority = rest.split(['/', '?', '#']).next().unwrap_or_default();
        if let Some((userinfo, _)) = authority.rsplit_once('@') {
            let http = scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https");
            // `ssh://git@host/...` is a plain user name; over HTTP(S) any
            // user info is usually an access token.
            if http || userinfo.contains(':') {
                return Err(StoreError::invalid(
                    "source location must not embed credentials; configure a git credential helper or a secret reference instead",
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locations_without_credentials_are_accepted() {
        for ok in [
            "/srv/repos/shop",
            "C:/work/shop",
            "https://example.com/org/shop.git",
            "ssh://git@example.com/org/shop.git",
            "git@example.com:org/shop.git",
        ] {
            assert!(validate_location(ok).is_ok(), "{ok}");
        }
    }

    #[test]
    fn credentials_are_rejected_without_echo() {
        let canary = "KNOWELL_CANARY_token_91c2";
        for bad in [
            format!("https://{canary}@example.com/org/shop.git"),
            format!("https://user:{canary}@example.com/org/shop.git"),
            format!("ssh://git:{canary}@example.com/org/shop.git"),
        ] {
            let err = validate_location(&bad).unwrap_err();
            assert!(!err.to_string().contains(canary));
            assert!(!format!("{err:?}").contains(canary));
        }
        assert!(validate_location("").is_err());
        assert!(validate_location("a\0b").is_err());
        assert!(validate_location(&"x".repeat(5000)).is_err());
    }
}
