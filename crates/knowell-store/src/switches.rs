//! Blue-green embedding profile switches and the profile each view serves.
//!
//! A view's *serving* profile is the one whose vectors its queries use. It
//! changes only through [`activate_switch`], which moves every member view of
//! a switch at once, after the target profile covers each view's active
//! generation. Until then queries keep the old profile, so a switch is never
//! a gap in semantic search.
//!
//! - [`record_configured_profile`] bootstraps a view's serving profile from
//!   its configuration and reports later configuration changes, which
//!   [`acknowledge_configured_profile`] records once they are handled.
//! - [`view_embeddings`] reads what several views serve in one statement.
//! - [`start_switch`], [`get_switch`], [`list_switches`],
//!   [`building_switch_of_view`], [`building_switches`].
//! - [`activate_switch`] (the atomic flip), [`cancel_switch`],
//!   [`start_rollback`] (a reverse switch within the retention window).

use std::collections::BTreeMap;

use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;

use crate::error::{StoreError, Violation, violation};
use crate::ids::{OrganizationId, ProfileId, ProfileSwitchId, ViewId, WorkspaceId};
use crate::types::{ProfileSwitchState, from_i64, to_i64};

/// Longest `requested_by` label, in bytes.
pub const MAX_REQUESTER_BYTES: usize = 256;

/// What a view serves and what its configuration named last.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ViewEmbedding {
    /// The view.
    pub view: ViewId,
    /// The profile whose vectors queries use; `None`: none.
    pub serving: Option<ProfileId>,
    /// The profile the configuration named at the last registration.
    pub configured: Option<ProfileId>,
    /// The switch that set `serving`, if one did.
    pub switch: Option<ProfileSwitchId>,
}

/// What [`record_configured_profile`] found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConfiguredProfile {
    /// First registration: the configured profile serves now.
    Recorded(ViewEmbedding),
    /// The configuration names the profile acknowledged before.
    Unchanged(ViewEmbedding),
    /// The configuration names another profile than the acknowledged one.
    /// Nothing changed: [`acknowledge_configured_profile`] records it once
    /// the change is handled (for example by starting a switch).
    Differs(ViewEmbedding),
}

/// Why a switch was started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub enum SwitchOrigin {
    /// An operator asked for it.
    Request,
    /// The configuration of the views changed.
    Configuration,
    /// It reverses an active switch.
    Rollback,
}

impl SwitchOrigin {
    /// The stored text form.
    pub fn as_str(&self) -> &'static str {
        match self {
            SwitchOrigin::Request => "request",
            SwitchOrigin::Configuration => "configuration",
            SwitchOrigin::Rollback => "rollback",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "request" => Ok(SwitchOrigin::Request),
            "configuration" => Ok(SwitchOrigin::Configuration),
            "rollback" => Ok(SwitchOrigin::Rollback),
            _ => Err(StoreError::Corrupt(
                "a profile switch has an unknown origin".to_owned(),
            )),
        }
    }
}

/// A switch to start.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NewSwitch {
    /// The organization.
    pub organization: OrganizationId,
    /// The workspace whose views move.
    pub workspace: WorkspaceId,
    /// The profile the views serve now (`None`: none).
    pub from: Option<ProfileId>,
    /// The profile they move to.
    pub to: ProfileId,
    /// Member views (at least one; all of `workspace`).
    pub views: Vec<ViewId>,
    /// Why ([`SwitchOrigin::Rollback`] only through [`start_rollback`]).
    pub origin: SwitchOrigin,
    /// Who asked: an audit label of `[A-Za-z0-9._:/@-]`, 1 to 256 bytes.
    pub requested_by: String,
    /// How long after activation a rollback is accepted, in seconds.
    pub retention_seconds: u64,
}

/// A stored switch.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProfileSwitch {
    /// Id.
    pub id: ProfileSwitchId,
    /// The organization.
    pub organization: OrganizationId,
    /// The workspace.
    pub workspace: WorkspaceId,
    /// Profile served before (`None`: none).
    pub from: Option<ProfileId>,
    /// Target profile.
    pub to: ProfileId,
    /// Why it was started.
    pub origin: SwitchOrigin,
    /// The switch a rollback reverses.
    pub rollback_of: Option<ProfileSwitchId>,
    /// Who asked.
    pub requested_by: String,
    /// Lifecycle state.
    pub state: ProfileSwitchState,
    /// Rollback window after activation, in seconds.
    pub retention_seconds: u64,
    /// Member views, ordered by id.
    pub views: Vec<ViewId>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Activation time.
    pub activated_at: Option<OffsetDateTime>,
    /// Until when a rollback is accepted.
    pub reversible_until: Option<OffsetDateTime>,
    /// When it was cancelled or rolled back.
    pub finished_at: Option<OffsetDateTime>,
}

/// What [`activate_switch`] did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SwitchActivation {
    /// The target serves every member view now.
    Activated(ProfileSwitch),
    /// These member views' active generations are not covered by the target
    /// yet; nothing changed.
    Pending(Vec<ViewId>),
    /// The switch is not building (activated, cancelled or rolled back
    /// before); nothing changed.
    NotBuilding(ProfileSwitch),
}

#[derive(sqlx::FromRow)]
struct ViewEmbeddingRow {
    view_id: ViewId,
    serving_profile_id: Option<ProfileId>,
    configured_profile_id: Option<ProfileId>,
    switch_id: Option<ProfileSwitchId>,
}

impl From<ViewEmbeddingRow> for ViewEmbedding {
    fn from(row: ViewEmbeddingRow) -> Self {
        ViewEmbedding {
            view: row.view_id,
            serving: row.serving_profile_id,
            configured: row.configured_profile_id,
            switch: row.switch_id,
        }
    }
}

#[derive(sqlx::FromRow)]
struct SwitchRow {
    id: ProfileSwitchId,
    organization_id: OrganizationId,
    workspace_id: WorkspaceId,
    from_profile_id: Option<ProfileId>,
    to_profile_id: ProfileId,
    origin: String,
    rollback_of: Option<ProfileSwitchId>,
    requested_by: String,
    state: ProfileSwitchState,
    retention_seconds: i64,
    created_at: OffsetDateTime,
    activated_at: Option<OffsetDateTime>,
    reversible_until: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
    views: Vec<ViewId>,
}

impl TryFrom<SwitchRow> for ProfileSwitch {
    type Error = StoreError;

    fn try_from(row: SwitchRow) -> Result<Self, StoreError> {
        Ok(ProfileSwitch {
            id: row.id,
            organization: row.organization_id,
            workspace: row.workspace_id,
            from: row.from_profile_id,
            to: row.to_profile_id,
            origin: SwitchOrigin::parse(&row.origin)?,
            rollback_of: row.rollback_of,
            requested_by: row.requested_by,
            state: row.state,
            retention_seconds: from_i64(row.retention_seconds, "switch retention")?,
            views: row.views,
            created_at: row.created_at,
            activated_at: row.activated_at,
            reversible_until: row.reversible_until,
            finished_at: row.finished_at,
        })
    }
}

/// Columns of a switch with its sorted member views; the switch table is `s`.
macro_rules! switch_select {
    () => {
        "SELECT s.id, s.organization_id, s.workspace_id, s.from_profile_id, s.to_profile_id,
                s.origin, s.rollback_of, s.requested_by, s.state, s.retention_seconds,
                s.created_at, s.activated_at, s.reversible_until, s.finished_at,
                ARRAY(SELECT m.view_id FROM profile_switch_view m
                      WHERE m.switch_id = s.id ORDER BY m.view_id) AS views
         FROM profile_switch s "
    };
}

fn check_requester(label: &str) -> Result<(), StoreError> {
    let valid = !label.is_empty()
        && label.len() <= MAX_REQUESTER_BYTES
        && label
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || "._:/@-".contains(c));
    if valid {
        Ok(())
    } else {
        Err(StoreError::invalid(
            "a switch requester must be 1-256 characters of A-Z, a-z, 0-9, '.', '_', ':', '/', '@' or '-'",
        ))
    }
}

/// Compares the profile the configuration names for `view` (`None`: it
/// embeds with none) with the acknowledged one. The first call records it
/// as both configured and serving; later calls change nothing.
pub async fn record_configured_profile(
    conn: &mut PgConnection,
    view: ViewId,
    configured: Option<ProfileId>,
) -> Result<ConfiguredProfile, StoreError> {
    let inserted = sqlx::query_as::<_, ViewEmbeddingRow>(
        "INSERT INTO view_embedding (view_id, serving_profile_id, configured_profile_id)
         VALUES ($1, $2, $2)
         ON CONFLICT (view_id) DO NOTHING
         RETURNING view_id, serving_profile_id, configured_profile_id, switch_id",
    )
    .bind(view)
    .bind(configured)
    .fetch_optional(&mut *conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::ForeignKey(_)) => StoreError::not_found("view or profile", view),
        _ => StoreError::Database(e),
    })?;
    if let Some(row) = inserted {
        return Ok(ConfiguredProfile::Recorded(row.into()));
    }
    let current: ViewEmbedding = sqlx::query_as::<_, ViewEmbeddingRow>(
        "SELECT view_id, serving_profile_id, configured_profile_id, switch_id
         FROM view_embedding WHERE view_id = $1",
    )
    .bind(view)
    .fetch_one(conn)
    .await?
    .into();
    Ok(if current.configured == configured {
        ConfiguredProfile::Unchanged(current)
    } else {
        ConfiguredProfile::Differs(current)
    })
}

/// Acknowledges `configured` as the profile the configuration names for
/// `view`. A view that served no profile serves it from now on (nothing older
/// can keep serving); a serving profile is changed only by a switch.
pub async fn acknowledge_configured_profile(
    conn: &mut PgConnection,
    view: ViewId,
    configured: Option<ProfileId>,
) -> Result<ViewEmbedding, StoreError> {
    let row = sqlx::query_as::<_, ViewEmbeddingRow>(
        "UPDATE view_embedding
         SET configured_profile_id = $2,
             serving_profile_id = coalesce(serving_profile_id, $2),
             updated_at = now()
         WHERE view_id = $1
         RETURNING view_id, serving_profile_id, configured_profile_id, switch_id",
    )
    .bind(view)
    .bind(configured)
    .fetch_optional(conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::ForeignKey(_)) => StoreError::not_found("profile", view),
        _ => StoreError::Database(e),
    })?;
    row.map(Into::into)
        .ok_or_else(|| StoreError::not_found("view embedding state", view))
}

/// What each of `views` serves, read in one statement (one snapshot, so a
/// concurrent [`activate_switch`] is seen for all of them or for none).
/// Views without a row are absent.
pub async fn view_embeddings(
    conn: &mut PgConnection,
    views: &[ViewId],
) -> Result<BTreeMap<ViewId, ViewEmbedding>, StoreError> {
    let rows = sqlx::query_as::<_, ViewEmbeddingRow>(
        "SELECT view_id, serving_profile_id, configured_profile_id, switch_id
         FROM view_embedding WHERE view_id = ANY($1)",
    )
    .bind(views)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(|row| (row.view_id, row.into()))
        .collect())
}

/// Starts a switch. Fails with [`StoreError::AlreadyExists`] while another
/// switch of the workspace is building, [`StoreError::NotFound`] when a view
/// or profile is not part of the workspace or organization, and
/// [`StoreError::InvalidInput`] for an empty view list, equal profiles or an
/// invalid requester.
pub async fn start_switch(
    conn: &mut PgConnection,
    new: &NewSwitch,
) -> Result<ProfileSwitch, StoreError> {
    if new.origin == SwitchOrigin::Rollback {
        return Err(StoreError::invalid(
            "rollback switches are started by start_rollback",
        ));
    }
    let mut tx = conn.begin().await?;
    let id = insert_switch(&mut tx, new, None).await?;
    let switch = read_switch(&mut tx, new.organization, id)
        .await?
        .ok_or_else(|| StoreError::Corrupt("a new profile switch vanished".to_owned()))?;
    tx.commit().await?;
    Ok(switch)
}

async fn insert_switch(
    conn: &mut PgConnection,
    new: &NewSwitch,
    rollback_of: Option<ProfileSwitchId>,
) -> Result<ProfileSwitchId, StoreError> {
    check_requester(&new.requested_by)?;
    let mut views = new.views.clone();
    views.sort();
    views.dedup();
    if views.is_empty() {
        return Err(StoreError::invalid(
            "a profile switch needs at least one view",
        ));
    }
    if new.from == Some(new.to) {
        return Err(StoreError::invalid(
            "a profile switch must move to another profile",
        ));
    }
    let members: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM view v
         JOIN project p ON p.id = v.project_id
         JOIN workspace w ON w.id = p.workspace_id
         WHERE v.id = ANY($1) AND w.id = $2 AND w.organization_id = $3",
    )
    .bind(&views)
    .bind(new.workspace)
    .bind(new.organization)
    .fetch_one(&mut *conn)
    .await?;
    if usize::try_from(members).ok() != Some(views.len()) {
        return Err(StoreError::not_found(
            "view of the workspace",
            new.workspace,
        ));
    }
    let profiles: Vec<ProfileId> = new.from.into_iter().chain([new.to]).collect();
    let known: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM embedding_profile WHERE id = ANY($1) AND organization_id = $2",
    )
    .bind(&profiles)
    .bind(new.organization)
    .fetch_one(&mut *conn)
    .await?;
    if usize::try_from(known).ok() != Some(profiles.len()) {
        return Err(StoreError::not_found("embedding profile", new.to));
    }
    let origin = if rollback_of.is_some() {
        SwitchOrigin::Rollback
    } else {
        new.origin
    };
    let id: ProfileSwitchId = sqlx::query_scalar(
        "INSERT INTO profile_switch (organization_id, workspace_id, from_profile_id, to_profile_id,
             origin, rollback_of, requested_by, retention_seconds)
         VALUES ($1, $2, $3, $4, $5, $6, $7, $8)
         RETURNING id",
    )
    .bind(new.organization)
    .bind(new.workspace)
    .bind(new.from)
    .bind(new.to)
    .bind(origin.as_str())
    .bind(rollback_of)
    .bind(&new.requested_by)
    .bind(to_i64(new.retention_seconds, "switch retention")?)
    .fetch_one(&mut *conn)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::Unique(_)) => StoreError::AlreadyExists {
            entity: "building profile switch of workspace",
            key: new.workspace.to_string(),
        },
        _ => StoreError::Database(e),
    })?;
    sqlx::query(
        "INSERT INTO profile_switch_view (switch_id, view_id) SELECT $1, unnest($2::uuid[])",
    )
    .bind(id)
    .bind(&views)
    .execute(&mut *conn)
    .await?;
    Ok(id)
}

async fn read_switch(
    conn: &mut PgConnection,
    organization: OrganizationId,
    id: ProfileSwitchId,
) -> Result<Option<ProfileSwitch>, StoreError> {
    let row = sqlx::query_as::<_, SwitchRow>(concat!(
        switch_select!(),
        "WHERE s.id = $1 AND s.organization_id = $2"
    ))
    .bind(id)
    .bind(organization)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// One switch of `organization`.
pub async fn get_switch(
    conn: &mut PgConnection,
    organization: OrganizationId,
    id: ProfileSwitchId,
) -> Result<Option<ProfileSwitch>, StoreError> {
    read_switch(conn, organization, id).await
}

/// The newest `limit` switches of `organization`, newest first.
pub async fn list_switches(
    conn: &mut PgConnection,
    organization: OrganizationId,
    limit: u32,
) -> Result<Vec<ProfileSwitch>, StoreError> {
    let rows = sqlx::query_as::<_, SwitchRow>(concat!(
        switch_select!(),
        "WHERE s.organization_id = $1 ORDER BY s.created_at DESC, s.id DESC LIMIT $2"
    ))
    .bind(organization)
    .bind(i64::from(limit.max(1)))
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// Every building switch of `organization` (to resume after a restart),
/// oldest first.
pub async fn building_switches(
    conn: &mut PgConnection,
    organization: OrganizationId,
) -> Result<Vec<ProfileSwitch>, StoreError> {
    let rows = sqlx::query_as::<_, SwitchRow>(concat!(
        switch_select!(),
        "WHERE s.organization_id = $1 AND s.state = 'building' ORDER BY s.created_at, s.id"
    ))
    .bind(organization)
    .fetch_all(conn)
    .await?;
    rows.into_iter().map(TryInto::try_into).collect()
}

/// The building switch `view` is a member of, if any (at most one: one
/// switch builds per workspace).
pub async fn building_switch_of_view(
    conn: &mut PgConnection,
    view: ViewId,
) -> Result<Option<ProfileSwitch>, StoreError> {
    let row = sqlx::query_as::<_, SwitchRow>(concat!(
        switch_select!(),
        "WHERE s.state = 'building'
           AND EXISTS (SELECT 1 FROM profile_switch_view m
                       WHERE m.switch_id = s.id AND m.view_id = $1)"
    ))
    .bind(view)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Makes the target serve every member view in one transaction, when it
/// covers each view's active generation with an active vector index
/// generation (a view without an active generation counts as covered).
/// Activating a rollback marks the switch it reverses `rolled_back`.
/// Takes the view row locks in id order, like generation activation.
pub async fn activate_switch(
    conn: &mut PgConnection,
    organization: OrganizationId,
    id: ProfileSwitchId,
) -> Result<SwitchActivation, StoreError> {
    let mut tx = conn.begin().await?;
    let state: Option<ProfileSwitchState> = sqlx::query_scalar(
        "SELECT state FROM profile_switch WHERE id = $1 AND organization_id = $2 FOR UPDATE",
    )
    .bind(id)
    .bind(organization)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(state) = state else {
        return Err(StoreError::not_found("profile switch", id));
    };
    if state != ProfileSwitchState::Building {
        let switch = read_switch(&mut tx, organization, id)
            .await?
            .ok_or_else(|| StoreError::not_found("profile switch", id))?;
        return Ok(SwitchActivation::NotBuilding(switch));
    }
    sqlx::query(
        "SELECT v.id FROM view v JOIN profile_switch_view m ON m.view_id = v.id
         WHERE m.switch_id = $1 ORDER BY v.id FOR UPDATE OF v",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let pending: Vec<ViewId> = sqlx::query_scalar(
        "SELECT v.id FROM view v
         JOIN profile_switch_view m ON m.view_id = v.id
         JOIN profile_switch s ON s.id = m.switch_id
         WHERE m.switch_id = $1 AND v.active_generation IS NOT NULL
           AND NOT EXISTS (
             SELECT 1 FROM index_generation g
             WHERE g.view_id = v.id AND g.profile_id = s.to_profile_id
               AND g.view_generation = v.active_generation AND g.state = 'active')
         ORDER BY v.id",
    )
    .bind(id)
    .fetch_all(&mut *tx)
    .await?;
    if !pending.is_empty() {
        return Ok(SwitchActivation::Pending(pending));
    }
    sqlx::query(
        "INSERT INTO view_embedding (view_id, serving_profile_id, switch_id)
         SELECT m.view_id, s.to_profile_id, s.id
         FROM profile_switch_view m JOIN profile_switch s ON s.id = m.switch_id
         WHERE m.switch_id = $1
         ON CONFLICT (view_id) DO UPDATE
           SET serving_profile_id = EXCLUDED.serving_profile_id,
               switch_id = EXCLUDED.switch_id,
               updated_at = now()",
    )
    .bind(id)
    .execute(&mut *tx)
    .await?;
    let rollback_of: Option<ProfileSwitchId> = sqlx::query_scalar(
        "UPDATE profile_switch
         SET state = 'active', activated_at = now(),
             reversible_until = now() + make_interval(secs => retention_seconds::double precision)
         WHERE id = $1
         RETURNING rollback_of",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if let Some(original) = rollback_of {
        sqlx::query(
            "UPDATE profile_switch SET state = 'rolled_back', finished_at = now()
             WHERE id = $1 AND state = 'active'",
        )
        .bind(original)
        .execute(&mut *tx)
        .await?;
    }
    let switch = read_switch(&mut tx, organization, id)
        .await?
        .ok_or_else(|| StoreError::Corrupt("an activated profile switch vanished".to_owned()))?;
    tx.commit().await?;
    Ok(SwitchActivation::Activated(switch))
}

/// Cancels a building switch; the old profile keeps serving. Fails with
/// [`StoreError::InvalidInput`] for a switch in another state.
pub async fn cancel_switch(
    conn: &mut PgConnection,
    organization: OrganizationId,
    id: ProfileSwitchId,
) -> Result<ProfileSwitch, StoreError> {
    let mut tx = conn.begin().await?;
    let done = sqlx::query(
        "UPDATE profile_switch SET state = 'cancelled', finished_at = now()
         WHERE id = $1 AND organization_id = $2 AND state = 'building'",
    )
    .bind(id)
    .bind(organization)
    .execute(&mut *tx)
    .await?;
    let switch = read_switch(&mut tx, organization, id)
        .await?
        .ok_or_else(|| StoreError::not_found("profile switch", id))?;
    if done.rows_affected() == 0 {
        return Err(StoreError::invalid(format!(
            "profile switch {id} is {}; only a building switch can be cancelled",
            switch.state
        )));
    }
    tx.commit().await?;
    Ok(switch)
}

/// Starts the switch that reverses the active switch `id`, back to the
/// profile it replaced, with the same views and retention. Fails with
/// [`StoreError::InvalidInput`] when `id` is not active, replaced no
/// profile, its rollback window has passed or a member view's serving
/// profile was set by something else since; [`StoreError::AlreadyExists`]
/// while another switch of the workspace is building.
pub async fn start_rollback(
    conn: &mut PgConnection,
    organization: OrganizationId,
    id: ProfileSwitchId,
    requested_by: &str,
) -> Result<ProfileSwitch, StoreError> {
    check_requester(requested_by)?;
    let mut tx = conn.begin().await?;
    sqlx::query("SELECT 1 FROM profile_switch WHERE id = $1 AND organization_id = $2 FOR UPDATE")
        .bind(id)
        .bind(organization)
        .execute(&mut *tx)
        .await?;
    let original = read_switch(&mut tx, organization, id)
        .await?
        .ok_or_else(|| StoreError::not_found("profile switch", id))?;
    if original.state != ProfileSwitchState::Active {
        return Err(StoreError::invalid(format!(
            "profile switch {id} is {}; only an active switch can be rolled back",
            original.state
        )));
    }
    let Some(back_to) = original.from else {
        return Err(StoreError::invalid(format!(
            "profile switch {id} replaced no profile; there is nothing to roll back to"
        )));
    };
    let open: bool =
        sqlx::query_scalar("SELECT reversible_until > now() FROM profile_switch WHERE id = $1")
            .bind(id)
            .fetch_one(&mut *tx)
            .await?;
    if !open {
        return Err(StoreError::invalid(format!(
            "the rollback window of profile switch {id} has passed"
        )));
    }
    let moved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM profile_switch_view m
         LEFT JOIN view_embedding e ON e.view_id = m.view_id
         WHERE m.switch_id = $1 AND e.switch_id IS DISTINCT FROM $1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    if moved > 0 {
        return Err(StoreError::invalid(format!(
            "views of profile switch {id} were switched again since; it cannot be rolled back"
        )));
    }
    let reverse = NewSwitch {
        organization,
        workspace: original.workspace,
        from: Some(original.to),
        to: back_to,
        views: original.views.clone(),
        origin: SwitchOrigin::Rollback,
        requested_by: requested_by.to_owned(),
        retention_seconds: original.retention_seconds,
    };
    let reverse_id = insert_switch(&mut tx, &reverse, Some(id)).await?;
    let switch = read_switch(&mut tx, organization, reverse_id)
        .await?
        .ok_or_else(|| StoreError::Corrupt("a new rollback switch vanished".to_owned()))?;
    tx.commit().await?;
    Ok(switch)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn requesters_are_audit_labels() {
        assert!(check_requester("user:alice@acme").is_ok());
        assert!(check_requester("configuration").is_ok());
        assert!(check_requester("").is_err());
        assert!(check_requester("has space").is_err());
        assert!(check_requester("ctrl\u{0}").is_err());
        assert!(check_requester(&"a".repeat(MAX_REQUESTER_BYTES + 1)).is_err());
    }

    #[test]
    fn origins_round_trip() {
        for origin in [
            SwitchOrigin::Request,
            SwitchOrigin::Configuration,
            SwitchOrigin::Rollback,
        ] {
            assert_eq!(SwitchOrigin::parse(origin.as_str()).unwrap(), origin);
        }
        assert!(SwitchOrigin::parse("other").is_err());
    }
}
