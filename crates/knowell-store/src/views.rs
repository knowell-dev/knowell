//! Views, generations with the activation fence, and view manifests.
//!
//! A view is one project following one track target. Every indexing run of
//! a view writes a new *generation*:
//!
//! 1. [`begin_generation`] allocates the next number (only one generation per
//!    view may be building at a time; the database enforces it).
//! 2. Writers add rows for that generation (files, occurrences, edges,
//!    contracts). Every write first takes a shared lock on the generation and
//!    checks it is still building — the *write fence*.
//! 3. [`activate_generation`] makes it the active one with a compare-and-set
//!    that only succeeds if it is newer than the active generation, so a late
//!    job of an old generation can never replace newer data.
//!    [`fail_generation`] abandons it and rolls its rows back.
//!
//! Generation-scoped rows are validity intervals: a row belongs to
//! generation `g` when `valid_from <= g < valid_to` (open-ended when
//! `valid_to` is `None`). Older generations stay readable until
//! [`prune_history`] removes rows no retained generation can see.

use std::collections::BTreeSet;

use knowell_core::{Name, TrackTarget};
use sqlx::{Connection, PgConnection};
use time::OffsetDateTime;

use crate::error::{StoreError, Violation, map_write, violation};
use crate::hierarchy::stored_name;
use crate::ids::{ManifestId, ProjectId, ViewId, WorkspaceId};
use crate::types::{GenerationState, ViewKind, truncate, validate_commit, validate_generation};

/// Longest error text stored with a failed generation, in bytes.
pub const MAX_ERROR_LEN: usize = 4096;

/// One project following one track target.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct View {
    /// Id.
    pub id: ViewId,
    /// Project.
    pub project: ProjectId,
    /// What the view follows.
    pub target: TrackTarget,
    /// Kind of target (derived from `target`).
    pub kind: ViewKind,
    /// Highest generation ever allocated (0 = none yet).
    pub last_generation: i64,
    /// Generation queries use by default, if any has been activated.
    pub active_generation: Option<i64>,
    /// Commit the active generation was built from (`None` for directories).
    pub active_commit: Option<String>,
    /// Newest commit seen on the tracked ref; may be ahead of `active_commit`.
    pub latest_seen_commit: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Last change of the counters or commits.
    pub updated_at: OffsetDateTime,
}

/// One indexing run of a view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewGeneration {
    /// View.
    pub view: ViewId,
    /// Generation number (positive, increasing per view).
    pub generation: i64,
    /// Commit indexed (`None` for directory sources).
    pub resolved_commit: Option<String>,
    /// Lifecycle state.
    pub state: GenerationState,
    /// Why it failed, for failed generations.
    pub error: Option<String>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// When it became active.
    pub activated_at: Option<OffsetDateTime>,
    /// When it stopped building (activated or failed) or was retired.
    pub finished_at: Option<OffsetDateTime>,
}

/// A view at a fixed generation: the unit every generation-aware read takes.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct GenerationPin {
    /// View.
    pub view: ViewId,
    /// Generation of that view.
    pub generation: i64,
}

/// Result of a successful activation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Activation {
    /// View.
    pub view: ViewId,
    /// The generation that is now active.
    pub generation: i64,
    /// The generation it replaced (now retired), if any.
    pub previous: Option<i64>,
}

/// A stored set of pins: one view generation per project.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ViewManifest {
    /// Id.
    pub id: ManifestId,
    /// Workspace.
    pub workspace: WorkspaceId,
    /// Name for named release views; `None` for ad-hoc query manifests.
    pub name: Option<Name>,
    /// Creation time.
    pub created_at: OffsetDateTime,
    /// Pins, ordered by project id.
    pub entries: Vec<ManifestEntry>,
}

impl ViewManifest {
    /// The pins of this manifest, ordered by project id.
    pub fn pins(&self) -> Vec<GenerationPin> {
        self.entries
            .iter()
            .map(|e| GenerationPin {
                view: e.view,
                generation: e.generation,
            })
            .collect()
    }
}

/// One project's pin in a manifest.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ManifestEntry {
    /// Project.
    pub project: ProjectId,
    /// View of that project.
    pub view: ViewId,
    /// Pinned generation.
    pub generation: i64,
    /// Commit of the pinned generation.
    pub resolved_commit: Option<String>,
}

/// What [`prune_history`] removed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct PruneSummary {
    /// The effective cutoff: generations below it are no longer readable.
    pub cutoff: i64,
    /// Interval rows deleted (files, occurrences, edges, contracts).
    pub rows_deleted: u64,
    /// Generation records deleted.
    pub generations_deleted: u64,
}

#[derive(sqlx::FromRow)]
struct ViewRow {
    id: ViewId,
    project_id: ProjectId,
    track_target: String,
    kind: ViewKind,
    last_generation: i64,
    active_generation: Option<i64>,
    active_commit: Option<String>,
    latest_seen_commit: Option<String>,
    created_at: OffsetDateTime,
    updated_at: OffsetDateTime,
}

impl TryFrom<ViewRow> for View {
    type Error = StoreError;

    fn try_from(row: ViewRow) -> Result<Self, StoreError> {
        let target = row
            .track_target
            .parse::<TrackTarget>()
            .map_err(|e| StoreError::Corrupt(format!("stored track target: {e}")))?;
        Ok(Self {
            id: row.id,
            project: row.project_id,
            target,
            kind: row.kind,
            last_generation: row.last_generation,
            active_generation: row.active_generation,
            active_commit: row.active_commit,
            latest_seen_commit: row.latest_seen_commit,
            created_at: row.created_at,
            updated_at: row.updated_at,
        })
    }
}

#[derive(sqlx::FromRow)]
struct GenerationRow {
    view_id: ViewId,
    generation: i64,
    resolved_commit: Option<String>,
    state: GenerationState,
    error: Option<String>,
    created_at: OffsetDateTime,
    activated_at: Option<OffsetDateTime>,
    finished_at: Option<OffsetDateTime>,
}

impl From<GenerationRow> for ViewGeneration {
    fn from(row: GenerationRow) -> Self {
        Self {
            view: row.view_id,
            generation: row.generation,
            resolved_commit: row.resolved_commit,
            state: row.state,
            error: row.error,
            created_at: row.created_at,
            activated_at: row.activated_at,
            finished_at: row.finished_at,
        }
    }
}

macro_rules! view_columns {
    () => {
        "id, project_id, track_target, kind, last_generation, active_generation, active_commit, latest_seen_commit, created_at, updated_at"
    };
}

macro_rules! generation_columns {
    () => {
        "view_id, generation, resolved_commit, state, error, created_at, activated_at, finished_at"
    };
}

/// The view kind a track target maps to.
pub fn view_kind(target: &TrackTarget) -> ViewKind {
    match target {
        TrackTarget::Branch(_) => ViewKind::Branch,
        TrackTarget::Remote { .. } => ViewKind::Remote,
        TrackTarget::Tag(_) => ViewKind::Tag,
        TrackTarget::Commit(_) => ViewKind::Commit,
        TrackTarget::WorktreeHead => ViewKind::Worktree,
    }
}

/// Creates a view of `project` following `target`. One view per
/// (project, target).
pub async fn create_view(
    conn: &mut PgConnection,
    project: ProjectId,
    target: &TrackTarget,
) -> Result<View, StoreError> {
    let text = target.to_string();
    sqlx::query_as::<_, ViewRow>(concat!(
        "INSERT INTO view (project_id, track_target, kind) VALUES ($1, $2, $3) RETURNING ",
        view_columns!()
    ))
    .bind(project)
    .bind(&text)
    .bind(view_kind(target))
    .fetch_one(conn)
    .await
    .map_err(|e| map_write(e, "view", &text, "project", project))?
    .try_into()
}

/// Looks a view up by id.
pub async fn get_view(conn: &mut PgConnection, id: ViewId) -> Result<Option<View>, StoreError> {
    let row = sqlx::query_as::<_, ViewRow>(concat!(
        "SELECT ",
        view_columns!(),
        " FROM view WHERE id = $1"
    ))
    .bind(id)
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Looks a view up by project and target.
pub async fn find_view(
    conn: &mut PgConnection,
    project: ProjectId,
    target: &TrackTarget,
) -> Result<Option<View>, StoreError> {
    let row = sqlx::query_as::<_, ViewRow>(concat!(
        "SELECT ",
        view_columns!(),
        " FROM view WHERE project_id = $1 AND track_target = $2"
    ))
    .bind(project)
    .bind(target.to_string())
    .fetch_optional(conn)
    .await?;
    row.map(TryInto::try_into).transpose()
}

/// Lists a project's views by target text.
pub async fn list_views(
    conn: &mut PgConnection,
    project: ProjectId,
) -> Result<Vec<View>, StoreError> {
    sqlx::query_as::<_, ViewRow>(concat!(
        "SELECT ",
        view_columns!(),
        " FROM view WHERE project_id = $1 ORDER BY track_target COLLATE \"C\""
    ))
    .bind(project)
    .fetch_all(conn)
    .await?
    .into_iter()
    .map(TryInto::try_into)
    .collect()
}

/// Deletes a view with all its generations and rows. Fails while a manifest
/// pins one of its generations. Returns whether it existed.
pub async fn delete_view(conn: &mut PgConnection, id: ViewId) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM view WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await
        .map_err(|e| match violation(&e) {
            Some(Violation::ForeignKey(_)) => StoreError::invalid(format!(
                "view {id} is pinned by a view manifest; delete the manifest first"
            )),
            _ => StoreError::Database(e),
        })?;
    Ok(done.rows_affected() > 0)
}

/// Records the newest commit seen on the view's tracked ref.
pub async fn record_seen_commit(
    conn: &mut PgConnection,
    view: ViewId,
    commit: &str,
) -> Result<(), StoreError> {
    validate_commit(commit)?;
    let done =
        sqlx::query("UPDATE view SET latest_seen_commit = $2, updated_at = now() WHERE id = $1")
            .bind(view)
            .bind(commit)
            .execute(conn)
            .await?;
    if done.rows_affected() == 0 {
        return Err(StoreError::not_found("view", view));
    }
    Ok(())
}

/// Allocates the next generation of `view` in state building and returns its
/// number. `resolved_commit` is the commit being indexed (`None` for
/// directory sources).
///
/// Fails with [`StoreError::GenerationBusy`] while another generation of the
/// view is building.
pub async fn begin_generation(
    conn: &mut PgConnection,
    view: ViewId,
    resolved_commit: Option<&str>,
) -> Result<i64, StoreError> {
    if let Some(commit) = resolved_commit {
        validate_commit(commit)?;
    }
    // A savepoint, so a rejected begin does not abort a caller's transaction
    // and the building generation can be reported.
    let mut tx = conn.begin().await?;
    let result = sqlx::query_scalar::<_, i64>(
        "WITH bumped AS (
           UPDATE view SET last_generation = last_generation + 1, updated_at = now()
           WHERE id = $1
           RETURNING id, last_generation
         )
         INSERT INTO view_generation (view_id, generation, resolved_commit)
         SELECT id, last_generation, $2 FROM bumped
         RETURNING generation",
    )
    .bind(view)
    .bind(resolved_commit)
    .fetch_optional(&mut *tx)
    .await;
    match result {
        Ok(Some(generation)) => {
            tx.commit().await?;
            Ok(generation)
        }
        Ok(None) => Err(StoreError::not_found("view", view)),
        Err(err) => {
            let busy = matches!(
                violation(&err),
                Some(Violation::Unique(Some(ref c))) if c == "view_generation_one_building"
            );
            tx.rollback().await?;
            if !busy {
                return Err(err.into());
            }
            match building_generation(conn, view).await? {
                Some(building) => Err(StoreError::GenerationBusy { view, building }),
                // The other generation finished in between; let the caller retry.
                None => Err(err.into()),
            }
        }
    }
}

/// The generation of `view` that is building, if any.
pub async fn building_generation(
    conn: &mut PgConnection,
    view: ViewId,
) -> Result<Option<i64>, StoreError> {
    Ok(sqlx::query_scalar::<_, i64>(
        "SELECT generation FROM view_generation WHERE view_id = $1 AND state = 'building'",
    )
    .bind(view)
    .fetch_optional(conn)
    .await?)
}

/// Looks one generation up.
pub async fn get_generation(
    conn: &mut PgConnection,
    view: ViewId,
    generation: i64,
) -> Result<Option<ViewGeneration>, StoreError> {
    let row = sqlx::query_as::<_, GenerationRow>(concat!(
        "SELECT ",
        generation_columns!(),
        " FROM view_generation WHERE view_id = $1 AND generation = $2"
    ))
    .bind(view)
    .bind(generation)
    .fetch_optional(conn)
    .await?;
    Ok(row.map(Into::into))
}

/// Lists a view's generations, newest first.
pub async fn list_generations(
    conn: &mut PgConnection,
    view: ViewId,
) -> Result<Vec<ViewGeneration>, StoreError> {
    let rows = sqlx::query_as::<_, GenerationRow>(concat!(
        "SELECT ",
        generation_columns!(),
        " FROM view_generation WHERE view_id = $1 ORDER BY generation DESC"
    ))
    .bind(view)
    .fetch_all(conn)
    .await?;
    Ok(rows.into_iter().map(Into::into).collect())
}

/// Makes `generation` the active generation of `view` and retires the
/// previous one.
///
/// The fence: the view row is updated with a compare-and-set that only
/// succeeds when `generation` is newer than the active generation, so a late
/// job of an older generation gets [`StoreError::StaleGeneration`] instead of
/// replacing newer data. A generation that is not building (failed, retired,
/// already active) gets [`StoreError::GenerationNotBuilding`].
pub async fn activate_generation(
    conn: &mut PgConnection,
    view: ViewId,
    generation: i64,
) -> Result<Activation, StoreError> {
    validate_generation(generation)?;
    let mut tx = conn.begin().await?;
    // Lock order: the view row first, then generation rows (as in
    // begin_generation and prune_history), so concurrent activations of one
    // view queue up instead of deadlocking on each other's generation rows.
    let active = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT active_generation FROM view WHERE id = $1 FOR UPDATE",
    )
    .bind(view)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| StoreError::not_found("view", view))?;
    // Exclusive lock on the generation: waits for in-flight fenced writes and
    // serializes with fail_generation.
    let state = sqlx::query_scalar::<_, GenerationState>(
        "SELECT state FROM view_generation WHERE view_id = $1 AND generation = $2 FOR UPDATE",
    )
    .bind(view)
    .bind(generation)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| StoreError::not_found("view generation", format!("{view}@{generation}")))?;

    // Compare-and-set in one statement.
    let swapped = sqlx::query(
        "UPDATE view v
         SET active_generation = $2, active_commit = g.resolved_commit, updated_at = now()
         FROM view_generation g
         WHERE v.id = $1
           AND g.view_id = v.id AND g.generation = $2 AND g.state = 'building'
           AND (v.active_generation IS NULL OR v.active_generation < $2)",
    )
    .bind(view)
    .bind(generation)
    .execute(&mut *tx)
    .await?
    .rows_affected();

    if swapped == 0 {
        return Err(match active {
            Some(active) if active >= generation => StoreError::StaleGeneration {
                view,
                generation,
                active,
            },
            _ => StoreError::GenerationNotBuilding {
                view,
                generation,
                state,
            },
        });
    }

    let previous = sqlx::query_scalar::<_, i64>(
        "UPDATE view_generation SET state = 'retired', finished_at = now()
         WHERE view_id = $1 AND state = 'active'
         RETURNING generation",
    )
    .bind(view)
    .fetch_optional(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE view_generation
         SET state = 'active', activated_at = now(), finished_at = now()
         WHERE view_id = $1 AND generation = $2",
    )
    .bind(view)
    .bind(generation)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Activation {
        view,
        generation,
        previous,
    })
}

/// Abandons a building generation: marks it failed with `error` (truncated to
/// [`MAX_ERROR_LEN`]; must not contain secrets) and rolls back every row it
/// wrote, so the next generation starts from the active data.
pub async fn fail_generation(
    conn: &mut PgConnection,
    view: ViewId,
    generation: i64,
    error: &str,
) -> Result<(), StoreError> {
    validate_generation(generation)?;
    let mut tx = conn.begin().await?;
    let state = sqlx::query_scalar::<_, GenerationState>(
        "SELECT state FROM view_generation WHERE view_id = $1 AND generation = $2 FOR UPDATE",
    )
    .bind(view)
    .bind(generation)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| StoreError::not_found("view generation", format!("{view}@{generation}")))?;
    if state != GenerationState::Building {
        return Err(StoreError::GenerationNotBuilding {
            view,
            generation,
            state,
        });
    }
    sqlx::query(
        "UPDATE view_generation SET state = 'failed', error = $3, finished_at = now()
         WHERE view_id = $1 AND generation = $2",
    )
    .bind(view)
    .bind(generation)
    .bind(truncate(error, MAX_ERROR_LEN))
    .execute(&mut *tx)
    .await?;
    // Undo in each table: drop rows born in the generation, then reopen rows
    // it closed (in this order, so a path never has two open rows).
    const ROLLBACK: &[&str] = &[
        "DELETE FROM file_version WHERE view_id = $1 AND valid_from = $2",
        "UPDATE file_version SET valid_to = NULL WHERE view_id = $1 AND valid_to = $2",
        "DELETE FROM occurrence WHERE view_id = $1 AND valid_from = $2",
        "UPDATE occurrence SET valid_to = NULL WHERE view_id = $1 AND valid_to = $2",
        "DELETE FROM edge WHERE view_id = $1 AND valid_from = $2",
        "UPDATE edge SET valid_to = NULL WHERE view_id = $1 AND valid_to = $2",
        "DELETE FROM contract WHERE view_id = $1 AND valid_from = $2",
        "UPDATE contract SET valid_to = NULL WHERE view_id = $1 AND valid_to = $2",
    ];
    for sql in ROLLBACK {
        sqlx::query(*sql)
            .bind(view)
            .bind(generation)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(())
}

/// The write fence: inside the caller's transaction, takes a shared lock on
/// the generation and checks that it is still building. Activation and
/// failure take an exclusive lock, so they wait for in-flight writes, and
/// writes that arrive afterwards are rejected.
pub(crate) async fn lock_building(
    conn: &mut PgConnection,
    view: ViewId,
    generation: i64,
) -> Result<(), StoreError> {
    validate_generation(generation)?;
    let state = sqlx::query_scalar::<_, GenerationState>(
        "SELECT state FROM view_generation WHERE view_id = $1 AND generation = $2 FOR SHARE",
    )
    .bind(view)
    .bind(generation)
    .fetch_optional(&mut *conn)
    .await?
    .ok_or_else(|| StoreError::not_found("view generation", format!("{view}@{generation}")))?;
    if state == GenerationState::Building {
        Ok(())
    } else {
        Err(StoreError::GenerationNotBuilding {
            view,
            generation,
            state,
        })
    }
}

/// The active generation of each view, read in one statement (a consistent
/// snapshot), ordered by view id. A view without an active generation is an
/// error, never skipped.
pub async fn active_pins(
    conn: &mut PgConnection,
    views: &[ViewId],
) -> Result<Vec<GenerationPin>, StoreError> {
    let ids: Vec<ViewId> = views
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let rows: Vec<(ViewId, Option<i64>)> =
        sqlx::query_as("SELECT id, active_generation FROM view WHERE id = ANY($1) ORDER BY id")
            .bind(&ids)
            .fetch_all(conn)
            .await?;
    let mut pins = Vec::with_capacity(rows.len());
    for id in &ids {
        match rows.iter().find(|(view, _)| view == id) {
            None => return Err(StoreError::not_found("view", id)),
            Some((_, None)) => {
                return Err(StoreError::invalid(format!(
                    "view {id} has no active generation yet"
                )));
            }
            Some((view, Some(generation))) => pins.push(GenerationPin {
                view: *view,
                generation: *generation,
            }),
        }
    }
    Ok(pins)
}

/// Stores a manifest pinning the active generation (and commit) of each of
/// `views`, all read in one statement. Every view must belong to a project of
/// `workspace`, have an active generation, and no two views may share a
/// project. `name` makes it a named release view (unique per workspace).
pub async fn pin_manifest(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    name: Option<&Name>,
    views: &[ViewId],
) -> Result<ViewManifest, StoreError> {
    let ids: Vec<ViewId> = views
        .iter()
        .copied()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if ids.is_empty() {
        return Err(StoreError::invalid(
            "a view manifest needs at least one view",
        ));
    }
    let mut tx = conn.begin().await?;
    let (id, created_at): (ManifestId, OffsetDateTime) = sqlx::query_as(
        "INSERT INTO view_manifest (workspace_id, name) VALUES ($1, $2) RETURNING id, created_at",
    )
    .bind(workspace)
    .bind(name.map(Name::as_str))
    .fetch_one(&mut *tx)
    .await
    .map_err(|e| {
        let key = name.map(Name::to_string).unwrap_or_default();
        map_write(e, "view manifest", key, "workspace", workspace)
    })?;
    let inserted = sqlx::query(
        "INSERT INTO view_manifest_entry (manifest_id, project_id, view_id, generation, resolved_commit)
         SELECT $1, v.project_id, v.id, v.active_generation, v.active_commit
         FROM view v JOIN project p ON p.id = v.project_id
         WHERE v.id = ANY($2) AND p.workspace_id = $3 AND v.active_generation IS NOT NULL",
    )
    .bind(id)
    .bind(&ids)
    .bind(workspace)
    .execute(&mut *tx)
    .await
    .map_err(|e| match violation(&e) {
        Some(Violation::Unique(_)) => {
            StoreError::invalid("a view manifest pins one view per project; two views share a project")
        }
        _ => StoreError::Database(e),
    })?
    .rows_affected();
    if usize::try_from(inserted).ok() != Some(ids.len()) {
        let found: Vec<(ViewId, WorkspaceId, Option<i64>)> = sqlx::query_as(
            "SELECT v.id, p.workspace_id, v.active_generation
             FROM view v JOIN project p ON p.id = v.project_id WHERE v.id = ANY($1)",
        )
        .bind(&ids)
        .fetch_all(&mut *tx)
        .await?;
        for view in &ids {
            match found.iter().find(|(id, _, _)| id == view) {
                None => return Err(StoreError::not_found("view", view)),
                Some((_, ws, _)) if *ws != workspace => {
                    return Err(StoreError::invalid(format!(
                        "view {view} does not belong to workspace {workspace}"
                    )));
                }
                Some((_, _, None)) => {
                    return Err(StoreError::invalid(format!(
                        "view {view} has no active generation yet"
                    )));
                }
                Some(_) => {}
            }
        }
        return Err(StoreError::Corrupt(
            "view manifest entries do not match the requested views".to_owned(),
        ));
    }
    let entries = manifest_entries(&mut tx, id).await?;
    tx.commit().await?;
    Ok(ViewManifest {
        id,
        workspace,
        name: name.cloned(),
        created_at,
        entries,
    })
}

async fn manifest_entries(
    conn: &mut PgConnection,
    id: ManifestId,
) -> Result<Vec<ManifestEntry>, StoreError> {
    let rows: Vec<(ProjectId, ViewId, i64, Option<String>)> = sqlx::query_as(
        "SELECT project_id, view_id, generation, resolved_commit
         FROM view_manifest_entry WHERE manifest_id = $1 ORDER BY project_id",
    )
    .bind(id)
    .fetch_all(conn)
    .await?;
    Ok(rows
        .into_iter()
        .map(
            |(project, view, generation, resolved_commit)| ManifestEntry {
                project,
                view,
                generation,
                resolved_commit,
            },
        )
        .collect())
}

/// Looks a manifest up by id.
pub async fn get_manifest(
    conn: &mut PgConnection,
    id: ManifestId,
) -> Result<Option<ViewManifest>, StoreError> {
    let row: Option<(WorkspaceId, Option<String>, OffsetDateTime)> =
        sqlx::query_as("SELECT workspace_id, name, created_at FROM view_manifest WHERE id = $1")
            .bind(id)
            .fetch_optional(&mut *conn)
            .await?;
    let Some((workspace, name, created_at)) = row else {
        return Ok(None);
    };
    let entries = manifest_entries(conn, id).await?;
    Ok(Some(ViewManifest {
        id,
        workspace,
        name: name.map(stored_name).transpose()?,
        created_at,
        entries,
    }))
}

/// Looks a named manifest (release view) up.
pub async fn find_manifest(
    conn: &mut PgConnection,
    workspace: WorkspaceId,
    name: &Name,
) -> Result<Option<ViewManifest>, StoreError> {
    let id: Option<ManifestId> =
        sqlx::query_scalar("SELECT id FROM view_manifest WHERE workspace_id = $1 AND name = $2")
            .bind(workspace)
            .bind(name.as_str())
            .fetch_optional(&mut *conn)
            .await?;
    match id {
        Some(id) => get_manifest(conn, id).await,
        None => Ok(None),
    }
}

/// Deletes a manifest (its pins stop protecting generations from pruning).
/// Returns whether it existed.
pub async fn delete_manifest(conn: &mut PgConnection, id: ManifestId) -> Result<bool, StoreError> {
    let done = sqlx::query("DELETE FROM view_manifest WHERE id = $1")
        .bind(id)
        .execute(conn)
        .await?;
    Ok(done.rows_affected() > 0)
}

/// Deletes history of `view` older than `keep_from`: interval rows no
/// generation `>= cutoff` can see, and retired or failed generation records
/// below the cutoff. The cutoff is lowered to the active generation and to
/// the oldest generation pinned by a manifest, so nothing readable through a
/// pin or the active view is ever removed.
pub async fn prune_history(
    conn: &mut PgConnection,
    view: ViewId,
    keep_from: i64,
) -> Result<PruneSummary, StoreError> {
    validate_generation(keep_from)?;
    let mut tx = conn.begin().await?;
    let active = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT active_generation FROM view WHERE id = $1 FOR UPDATE",
    )
    .bind(view)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or_else(|| StoreError::not_found("view", view))?;
    let pinned = sqlx::query_scalar::<_, Option<i64>>(
        "SELECT min(generation) FROM view_manifest_entry WHERE view_id = $1",
    )
    .bind(view)
    .fetch_one(&mut *tx)
    .await?;
    // Without an active generation, everything that exists may still be
    // needed by the generation being built on top of it.
    let Some(active) = active else {
        tx.commit().await?;
        return Ok(PruneSummary::default());
    };
    let cutoff = [Some(keep_from), Some(active), pinned]
        .into_iter()
        .flatten()
        .min()
        .unwrap_or(keep_from);
    const PRUNE: &[&str] = &[
        "DELETE FROM file_version WHERE view_id = $1 AND valid_to IS NOT NULL AND valid_to <= $2",
        "DELETE FROM occurrence WHERE view_id = $1 AND valid_to IS NOT NULL AND valid_to <= $2",
        "DELETE FROM edge WHERE view_id = $1 AND valid_to IS NOT NULL AND valid_to <= $2",
        "DELETE FROM contract WHERE view_id = $1 AND valid_to IS NOT NULL AND valid_to <= $2",
    ];
    let mut rows_deleted = 0;
    for sql in PRUNE {
        rows_deleted += sqlx::query(*sql)
            .bind(view)
            .bind(cutoff)
            .execute(&mut *tx)
            .await?
            .rows_affected();
    }
    let generations_deleted = sqlx::query(
        "DELETE FROM view_generation g
         WHERE g.view_id = $1 AND g.generation < $2 AND g.state IN ('retired', 'failed')
           AND NOT EXISTS (SELECT 1 FROM view_manifest_entry m
                           WHERE m.view_id = g.view_id AND m.generation = g.generation)",
    )
    .bind(view)
    .bind(cutoff)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    tx.commit().await?;
    Ok(PruneSummary {
        cutoff,
        rows_deleted,
        generations_deleted,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn view_kind_follows_target() {
        let cases = [
            ("branch:main", ViewKind::Branch),
            ("remote:origin/main", ViewKind::Remote),
            ("tag:v1.0.0", ViewKind::Tag),
            ("worktree", ViewKind::Worktree),
        ];
        for (text, kind) in cases {
            let target: TrackTarget = text.parse().unwrap();
            assert_eq!(view_kind(&target), kind, "{text}");
        }
        let commit: TrackTarget = format!("commit:{}", "a".repeat(40)).parse().unwrap();
        assert_eq!(view_kind(&commit), ViewKind::Commit);
    }

    #[test]
    fn manifest_pins_follow_entries() {
        let view = ViewId(uuid::Uuid::nil());
        let manifest = ViewManifest {
            id: ManifestId(uuid::Uuid::nil()),
            workspace: WorkspaceId(uuid::Uuid::nil()),
            name: None,
            created_at: OffsetDateTime::UNIX_EPOCH,
            entries: vec![ManifestEntry {
                project: ProjectId(uuid::Uuid::nil()),
                view,
                generation: 4,
                resolved_commit: None,
            }],
        };
        assert_eq!(
            manifest.pins(),
            vec![GenerationPin {
                view,
                generation: 4
            }]
        );
    }
}
